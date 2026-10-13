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

//! Contract **A.3** — the NIF C ABI, widened: a second block of live `enif_*` cases.
//!
//! A continuation of [`super::nif_abi`]: each case drives one `enif_*` behaviour end to end
//! through a real C `.so` loaded by `erlang:load_nif/2` and compares both runtimes. Case ids
//! use the `nif_extra/…` prefix. See the node `CONTEXT.md` ("How to add a corpus case").
//!
//! # Shape (identical to [`super::nif_abi`])
//!
//! A **single** C library with two `ErlNifFunc` entries; the embedded module `nif_extra` loads it
//! next to the compiled `.beam` (the self-locating trick the other A.3 groups use) and calls
//! `nif_extra:classify(N)`, which forwards the integer selector `N` to one of the two entries —
//! each through an **external** call, so the Rust runtime intercepts it at external dispatch (a
//! bare local call would keep running the `erlang:nif_error/1` stub):
//!
//! * `probe/1(Selector)` — the original entry: the C builds every term it needs with the public
//!   `enif_*` constructors (selectors `1`..`95` — see the second widening below);
//! * `probe_term/2(Selector, Term)` — the term-driven entry: the C observes a term the module
//!   built in Erlang (selectors `>= 100`), which the NIF C API cannot build itself — there is no
//!   bitstring constructor, so a **non-byte-aligned bitstring** (and a window of one) can only be
//!   built on the Erlang side.
//!
//! Each C branch returns a **plain integer** (`1` matched, `0` not, `-1` unknown selector, and for
//! the direct observations below either the accessor's own integer answer or a `-100` "the shape
//! did not match" sentinel), so every golden is a VM-stable integer line. Two selectors
//! deliberately raise from C; the module maps the raise class to an integer.
//!
//! # Why these selectors, and no others
//!
//! Both runtimes `dlopen(RTLD_NOW)` the **same** `.so`, so every `enif_*` the C references must be
//! a symbol the release `beam_rt` really exports (verified against `nm -D`); a symbol it does not
//! export fails the *Rust* load while the oracle loads it — a **mismatch**. Nothing C-variadic
//! (`enif_make_tuple`, `enif_make_list`, `enif_fprintf`, …) is referenced; the non-variadic
//! `*_from_array` / `enif_make_list_cell` forms stand in.
//!
//! The families whose host hook the runtime may leave at its trait default (`NifHost` in
//! `beam_abi`, implemented by `beam_rt`) are covered **only** by the shapes both runtimes answer
//! alike — a *plain differing value* is a mismatch, and a mismatch is forbidden here:
//!
//! * the get/make **boundary** shapes (`enif_get_int64` rejecting the `2^63` written by
//!   `enif_make_uint64`, `enif_get_uint`/`enif_get_uint64` rejecting an out-of-range / negative
//!   value, `INT64_MIN` round-tripping, `enif_get_double` on `1.0e308`);
//! * the list / tuple / binary **edges** (`enif_get_list_length` refusing an improper list,
//!   `enif_get_list_cell` refusing `[]`, the `[]` tail, the empty tuple / empty binary, a
//!   full-range `enif_make_sub_binary`, `enif_inspect_iolist_as_binary` over a flat binary and
//!   over `[]`);
//! * the **map** refusals (`enif_get_map_value` on a missing key and on a non-map,
//!   `enif_get_map_size` on a non-map, `enif_make_map_put`/`_remove` on a non-map,
//!   `enif_make_map_remove` of an absent key, `enif_make_map_from_arrays` with zero pairs) and
//!   the empty-map iterator head/tail;
//! * **`enif_term_type`** across the float / pid / `[]` lattice, **`enif_compare`** across the
//!   type rank (atom < tuple, list < bitstring), **`enif_is_identical`** on structurally equal
//!   containers, and **`enif_is_number`** on a bignum;
//! * the **string** edges (a list-of-codes input, the empty string, `enif_get_string_length([])`,
//!   an improper list and a non-byte element) and the atom read buffer bound;
//! * **`enif_term_size`** of a tuple / `[]` / an atom, and **`enif_hash`**`(ERL_NIF_PHASH2)`'s
//!   documented properties on a binary;
//! * the **exception** slot lifting a non-atom reason into Erlang, plus a clean process env;
//! * the deterministic **`enif_convert_time_unit`** pair and **`enif_monotonic_time`**'s
//!   monotonicity (its absolute value is documented to differ and is never asserted).
//!
//! A second widening (selectors `63`..`83`, still on `probe/1`) extends the same families with the
//! shapes the first block left out, every one of them answered alike by both runtimes:
//!
//! * the remaining **`enif_term_type`** lattice points (a bignum, a cons cell, a two-entry map, a
//!   tuple) and three more **`enif_compare`** points (an integer against the equal float, two atom
//!   names, a tuple before a map);
//! * **`enif_is_pid`** on the `enif_make_pid(enif_self)` term and **`enif_is_map`** on a map that
//!   holds a pair;
//! * the other **integer boundary** pairs (`UINT_MAX`, `LONG_MIN`, `INT_MIN`, `ULONG_MAX` round
//!   trips and `enif_get_int` refusing `INT_MIN - 1`);
//! * **`enif_make_map_from_arrays`** with two distinct pairs and **`enif_make_map_put`**
//!   overwriting an existing key (the size stays put, the value is replaced);
//! * the **string/atom buffer** edges: an exactly-fitting `enif_get_string` buffer, its refusal of
//!   a binary subject, a **UTF-8** atom name read back under `ERL_NIF_UTF8` (and refused under
//!   `ERL_NIF_LATIN1`, the atom not being Latin-1 representable) plus its
//!   `enif_get_atom_length(ERL_NIF_UTF8)` byte count;
//! * one further **`enif_convert_time_unit`** pair (ns -> us).
//!
//! `enif_compare` in that widening ranks the way the oracle does: it uses the C's arithmetic `CMP`,
//! so `enif_compare(1, 1.0)` is `0` (selector `67`), and an atom pair orders by name
//! (`enif_compare('alpha', 'beta')` is `-1`, selector `68`) — both adopted and pinned. The one
//! retained residual is a *nested* (tuple/list/map) atom pair, which still falls back to
//! `beam_term` index order; it is recorded as a divergence, never a corpus case.
//!
//! Not pinned anywhere (a real, documented divergence — recorded as a gap, never a corpus case):
//! the atom-cache slots, `enif_term_to_binary` on a valid term, `enif_binary_to_term` on a valid
//! encoding, `enif_make_ref` on a valid env, `enif_make_existing_atom` on a *committed* atom and
//! `enif_whereis_pid` on a name a process **is** registered under (see [`super::nif_abi`] for the
//! full list and owners).
//!
//! # The A.3 binary/string shape fixes, pinned
//!
//! A later selector block (the term-driven `probe_term/2` entry, selectors `101`..`143`) pins the
//! binary/string shape rules the runtime got wrong before, so they can never regress:
//!
//! * **`enif_inspect_iolist_as_binary`** follows the C's positional rules
//!   (`erts_iolist_size`, `Unit = 8`): an integer is a byte **only in the head of a cons cell**, so
//!   a bare byte integer (`65`), an improper-list tail (`[1|2]`), a non-byte integer head (`[256]`)
//!   and a non-byte-aligned bitstring (`[<<1:7>>]`) are all **refused** (`0`) — the bare byte
//!   integer was the long-standing divergence, now closed. `[]` (the same term as `nil`) is
//!   *accepted* with size `0`, and valid nested lists / byte-aligned bitstrings
//!   (`[1,[2,<<3>>]]`, `[<<1:8>>]`) flatten to the correct size and bytes.
//! * **`enif_get_string` under `ERL_NIF_UTF8`** drops a multibyte character that does not fit the
//!   byte budget **whole** (never emitting a truncated sequence) and answers the negative required
//!   length: `[16#61,16#20AC]` fits exactly in a 5-byte buffer (answer `5`, bytes
//!   `61 e2 82 ac`), is `-2` one byte smaller (the ASCII prefix written, the `€` dropped whole),
//!   and `[16#20AC]` alone is `-1` in a 3-byte buffer.
//! * **`enif_inspect_binary`** accepts a **byte-sized** bitstring whose start is not byte-aligned
//!   (answering `1` with the window's real bytes), while still refusing a **non-byte-sized** one
//!   (`<<1:7>>`).
//! * **`enif_make_sub_binary`** windows a bit-offset source by **bits** and answers the empty
//!   binary for a **zero-size** request at *any* position — including one past the last byte.
//!
//! Honest caveat on the bit-offset shapes (`110`, `120`, `121`): the two runtimes represent the
//! window differently — the oracle's `bs_get_binary2` yields a sub-binary that *references* the
//! source (a refc/off-heap window, so the source stays shared), while this runtime's `bs_match`
//! copies the bits into a byte-aligned on-heap bitstring (`beam_engine`'s `BsGetBinary2`, and
//! `erts_debug:unaligned_bitstring/2`'s documented representational deviation). The **observable**
//! is identical either way, so the cases pin the byte size and the shifted bytes through
//! `enif_inspect_binary` / `enif_make_sub_binary` rather than the internal offset.
//!
//! # The widened accessor / env / update block, pinned
//!
//! A third selector block (added after `83`) widens the *non-variadic* constructor and accessor
//! surface the two entries already reach:
//!
//! * the **length-taking constructors** — `enif_make_atom_len` interns the same atom as the
//!   NUL-terminated `enif_make_atom` (`84`), `enif_make_existing_atom_len` **refuses** a name the
//!   atom table does not hold (`85`, a pure lookup on both sides) and `enif_make_string_len` builds
//!   the list from exactly its length argument, so `"hello"` with length `3` reads back as `"hel"`
//!   (`86`);
//! * **`enif_make_copy` across environments** — a 2-tuple built in the call env and copied into a
//!   fresh `enif_alloc_env` environment keeps its arity and both elements (`87`);
//! * the **local identity accessors** — `enif_get_local_pid` round trips the pid `enif_make_pid`
//!   writes for `enif_self` (`88`) and refuses a non-pid (`89`), `enif_get_local_port` refuses a
//!   non-port (`90`);
//! * the **negative predicate lattice** — `enif_is_ref`, `enif_is_fun` and `enif_is_port` all
//!   refuse a 2-tuple (`91`), and `enif_is_exception` is `0` for an ordinary term with no raise
//!   pending (`92`);
//! * **`enif_make_map_update`** — an update-only sibling of `enif_make_map_put`: it refuses a key
//!   the map does not hold (`93`) and replaces an existing value while keeping the size at `1`
//!   (`94`);
//! * the **thread handle** — `enif_equal_tids` holds for the tid `enif_thread_self` reports
//!   (`95`).
//!
//! The term-driven block is widened in step (selectors `133`..`143`): the byte-shape rules the
//! `101`..`132` block pins are extended to the empty binary, to binary **ordering** and
//! self-identity, and to the remaining accepted iolist elements (`[0]`, `[255]`, `[<<>>]`,
//! `[[],[]]`, nested numeric lists, a binary element followed by a byte) plus one
//! `enif_make_sub_binary` window.
//!
//! # Skip policy (inherited from [`super::nif_so`])
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_nif.h`; when either is missing the
//! harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`).

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: the library's two `ErlNifFunc` stubs (`probe/1` and `probe_term/2`, both
/// exported so the oracle accepts the library) plus the `classify/1` driver.
///
/// `classify/1` loads the library next to this module's `.beam` and, on a successful load,
/// forwards the selector to one of the two entry points through an **external** call. Only an
/// integer leaves `run/1`: a NIF raise is caught and mapped to a class code, and any non-integer
/// return is mapped to `90`, so the module's observable is always one integer line.
const NIF_EXTRA_MODULE_SOURCE: &str = r#"-module(nif_extra).
-export([classify/1, probe/1, probe_term/2]).

%% The library's two ErlNifFunc entries: stubbed and exported so erlang:load_nif/2 accepts the
%% library, and dead code once the load succeeds (the real implementations are in the .so).
probe(_) -> erlang:nif_error(not_loaded).
probe_term(_, _) -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_extra", 0) of
        ok -> run(N);
        {error, {Reason, _Text}} -> reason_code(Reason)
    end.

%% The selector >= 100 block is term-driven: the module builds the term here and hands it to the
%% library's second entry point. The C API has no bitstring constructor, so a non-byte-aligned
%% bitstring — and an improper list, which the constructors cannot express either — can only be
%% built on this side. Every other selector stays on the integer-driven probe/1.
dispatch(N) when N >= 100 -> nif_extra:probe_term(N, built_term(N));
dispatch(N) -> nif_extra:probe(N).

%% The term each term-driven selector observes.
built_term(101) -> 65;                            %% a bare byte integer (not an iolist)
built_term(102) -> [1 | 2];                       %% an improper list
built_term(103) -> [256];                         %% a non-byte integer head
built_term(104) -> [<<1:7>>];                     %% a bit-sized (non-byte-aligned) element
built_term(105) -> [];                            %% the empty list
built_term(106) -> [1, [2, <<3>>]];               %% nested list + byte-aligned bitstring
built_term(107) -> [<<1:8>>];                     %% a one-byte bitstring element
built_term(108) -> [[]];                          %% a byte-less nested list
built_term(109) -> [1 | <<2>>];                   %% the binary tail rule
built_term(110) -> bit_window();                  %% enif_inspect_binary over a bit-offset window
built_term(111) -> <<1:7>>;                       %% a non-byte-sized bitstring
built_term(120) -> bit_window();                  %% enif_make_sub_binary(pos 0) of a bit-offset source
built_term(121) -> bit_window();                  %% enif_make_sub_binary(pos 4) of a bit-offset source
built_term(122) -> <<"abcdef">>;                  %% a zero-size sub-binary request
built_term(123) -> <<"abcdef">>;                  %% a zero-size request one past the last byte
built_term(130) -> [16#61, 16#20AC];              %% the UTF-8 truncation boundary (fits)
built_term(131) -> [16#61, 16#20AC];              %% the UTF-8 truncation boundary (one byte short)
built_term(132) -> [16#20AC];                     %% a lone multibyte char
built_term(133) -> <<1:7>>;                       %% a non-byte-aligned bitstring
built_term(134) -> <<>>;                          %% the empty bitstring
built_term(135) -> <<1,2,3>>;                     %% an ordered-binary compare
built_term(136) -> <<1,2,3>>;                     %% a self-compare
built_term(137) -> [[1,2],[3,4]];                 %% nested numeric lists
built_term(138) -> [<<"ab">>,99];                 %% a binary element then a byte
built_term(139) -> [0];                           %% the zero byte
built_term(140) -> [255];                         %% the highest byte
built_term(141) -> [<<>>];                        %% a byte-less binary element
built_term(142) -> [[],[]];                       %% two byte-less nested lists
built_term(143) -> <<"abcd">>.                    %% a sub-binary window source

%% A 40-byte window that starts five bits into `<<0,1,…,45>>` — a byte-sized bitstring with a
%% non-zero bit offset, exactly the shape enif_inspect_binary used to refuse.
bit_window() ->
    Bin = list_to_binary(lists:seq(0, 45)),
    <<_:5, W:40/binary, _/bits>> = Bin,
    W.

%% Every branch answers an integer, so the golden is a VM-stable integer line. The raising
%% selectors land in the explicit class clauses; the wildcard keeps a surprising failure a
%% diffable integer rather than an escaping raise.
run(N) ->
    try dispatch(N) of
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
const NIF_EXTRA_C: &str = r#"/* A second real NIF exercising the enif_* C ABI end to end (contract A.3). */
#include <erl_nif.h>
#include <limits.h>
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

/* The empty list, built through the non-variadic constructor. */
static ERL_NIF_TERM make_nil(ErlNifEnv* env) {
    ERL_NIF_TERM none[1] = {0};
    return enif_make_list_from_array(env, none, 0);
}

/* ------------------------------------------------------------------- term-driven helpers */

/* The flattened byte view of `term` as `enif_inspect_iolist_as_binary` reports it:
 * `-1` when the C refused the term (its return was `0`), else the flattened byte count
 * (`0` for the accepted empty iolist). `checksum` (when non-NULL) receives the order-sensitive
 * byte checksum `sum((i + 1) * byte[i])` of the accepted view — `0` for an empty view.
 * The view is only read, never released (the `probe/1` arms keep the same contract). */
static int iolist_shape(ErlNifEnv* env, ERL_NIF_TERM term, unsigned long long* checksum) {
    ErlNifBinary v;
    unsigned long long sum = 0;
    size_t i;

    if (!enif_inspect_iolist_as_binary(env, term, &v))
        return -1;
    for (i = 0; i < v.size; i++)
        sum += (unsigned long long) (i + 1) * v.data[i];
    if (checksum != NULL)
        *checksum = sum;
    return (int) v.size;
}

/* The byte view of `term` as `enif_inspect_binary` reports it, encoded as
 * `size * 1000000 + sum((i + 1) * byte[i])` so a wrong size and wrong bytes are both visible;
 * `-1` when the C refused the term. */
static long long binary_view(ErlNifEnv* env, ERL_NIF_TERM term) {
    ErlNifBinary v;
    unsigned long long sum = 0;
    size_t i;

    if (!enif_inspect_binary(env, term, &v))
        return -1;
    for (i = 0; i < v.size; i++)
        sum += (unsigned long long) (i + 1) * v.data[i];
    return (long long) v.size * 1000000LL + (long long) sum;
}

/* `1` when `term` is the *accepted empty* iolist: `enif_inspect_iolist_as_binary` answered `1`,
 * the size is `0`, and the view carries the C's **non-NULL** dummy `data` (`erl_nif.c:1392`:
 * `bin->data = (unsigned char *)&bin->data;`), else `0`. */
static int iolist_is_empty(ErlNifEnv* env, ERL_NIF_TERM term) {
    ErlNifBinary v;
    v.size = 0;
    v.data = NULL;
    v.ref_bin = NULL;
    if (!enif_inspect_iolist_as_binary(env, term, &v))
        return 0;
    return (v.size == 0 && v.data != NULL) ? 1 : 0;
}

/* `enif_make_sub_binary(env, term, pos, size)` inspected through `enif_inspect_binary`, encoded
 * like `binary_view`. */
static long long sub_binary_view(ErlNifEnv* env, ERL_NIF_TERM term, size_t pos, size_t size) {
    return binary_view(env, enif_make_sub_binary(env, term, pos, size));
}

static ERL_NIF_TERM probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int sel = 0;
    if (argc != 1 || !enif_get_int(env, argv[0], &sel))
        return enif_make_badarg(env);

    switch (sel) {
    /* --------------------------------------------------------------- integer boundaries */
    case 1: {                                  /* get_int64 refuses the positive 2^63 */
        ERL_NIF_TERM t = enif_make_uint64(env, (ErlNifUInt64) 1 << 63);
        ErlNifSInt64 s = 0;
        ErlNifUInt64 u = 0;
        int bad = enif_get_int64(env, t, &s);
        int ok = enif_get_uint64(env, t, &u) && u == ((ErlNifUInt64) 1 << 63);
        return enif_make_int(env, (!bad && ok) ? 1 : 0);
    }
    case 2: {                                  /* get_uint64 refuses a negative integer */
        ErlNifUInt64 u = 0;
        return enif_make_int(env, enif_get_uint64(env, enif_make_int(env, -1), &u) ? 0 : 1);
    }
    case 3: {                                  /* get_uint refuses a value above UINT_MAX */
        ERL_NIF_TERM t = enif_make_int64(env, (ErlNifSInt64) 1 << 35);
        unsigned v = 0;
        return enif_make_int(env, enif_get_uint(env, t, &v) ? 0 : 1);
    }
    case 4: {                                  /* int64 INT64_MIN round trips */
        ErlNifSInt64 want = (ErlNifSInt64) (-9223372036854775807LL - 1);
        ERL_NIF_TERM t = enif_make_int64(env, want);
        ErlNifSInt64 v = 0;
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v == want) ? 1 : 0);
    }
    case 5: {                                  /* get_double round trips a large magnitude */
        ERL_NIF_TERM t = enif_make_double(env, 1.0e308);
        double v = 0.0;
        return enif_make_int(env, (enif_get_double(env, t, &v) && v == 1.0e308) ? 1 : 0);
    }
    case 6: {                                  /* get_uint refuses a float */
        unsigned v = 0;
        return enif_make_int(env, enif_get_uint(env, enif_make_double(env, 1.0), &v) ? 0 : 1);
    }
    case 7: {                                  /* get_long refuses a non-number */
        long v = 0;
        return enif_make_int(env, enif_get_long(env, enif_make_atom(env, "not_a_long"), &v) ? 0 : 1);
    }
    case 8: {                                  /* get_ulong refuses a negative integer */
        unsigned long v = 0;
        return enif_make_int(env, enif_get_ulong(env, enif_make_int(env, -7), &v) ? 0 : 1);
    }

    /* -------------------------------------------------------------------------- lists */
    case 9: {                                  /* get_list_length refuses an improper list */
        ERL_NIF_TERM cell = enif_make_list_cell(env, enif_make_int(env, 1),
                                               enif_make_int(env, 2));   /* [1|2] */
        unsigned n = 0;
        return enif_make_int(env, enif_get_list_length(env, cell, &n) ? 0 : 1);
    }
    case 10: {                                 /* get_list_cell refuses the empty list */
        ERL_NIF_TERM h = 0, t = 0;
        return enif_make_int(env, enif_get_list_cell(env, make_nil(env), &h, &t) ? 0 : 1);
    }
    case 11: {                                 /* the tail of a 1-element list is [] */
        ERL_NIF_TERM arr[1];
        ERL_NIF_TERM h = 0, t = 0;
        arr[0] = enif_make_int(env, 5);
        if (!enif_get_list_cell(env, enif_make_list_from_array(env, arr, 1), &h, &t))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_empty_list(env, t) ? 1 : 0);
    }
    case 12: {                                 /* is_list([]) true; is_empty_list([1]) false */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            (enif_is_list(env, make_nil(env)) &&
             !enif_is_empty_list(env, enif_make_list_from_array(env, arr, 1))) ? 1 : 0);
    }
    case 13: {                                 /* make_reverse_list of [] is [] */
        ERL_NIF_TERM rev = 0;
        if (!enif_make_reverse_list(env, make_nil(env), &rev))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_empty_list(env, rev) ? 1 : 0);
    }
    case 14: {                                 /* get_list_length of a 3-element list */
        ERL_NIF_TERM arr[3];
        unsigned n = 0;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        return enif_make_int(env,
            (enif_get_list_length(env, enif_make_list_from_array(env, arr, 3), &n) && n == 3)
                ? 1 : 0);
    }

    /* ------------------------------------------------------------------------- tuples */
    case 15: {                                 /* a 1-tuple: arity and element */
        ERL_NIF_TERM arr[1];
        int arity = -1, v = 0;
        const ERL_NIF_TERM* got = NULL;
        arr[0] = enif_make_int(env, 5);
        if (!enif_get_tuple(env, enif_make_tuple_from_array(env, arr, 1), &arity, &got))
            return enif_make_int(env, 0);
        if (!enif_get_int(env, got[0], &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (arity == 1 && v == 5) ? 1 : 0);
    }
    case 16: {                                 /* is_tuple({}) true and its arity 0 */
        ERL_NIF_TERM none[1] = {0};
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        ERL_NIF_TERM tup = enif_make_tuple_from_array(env, none, 0);
        if (!enif_is_tuple(env, tup))
            return enif_make_int(env, 0);
        if (!enif_get_tuple(env, tup, &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity == 0 ? 1 : 0);
    }
    case 17: {                                 /* get_tuple arity of a 2-tuple */
        ERL_NIF_TERM arr[2];
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        if (!enif_get_tuple(env, enif_make_tuple_from_array(env, arr, 2), &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity);
    }

    /* ----------------------------------------------------------------------- binaries */
    case 18: {                                 /* inspect the empty binary */
        ErlNifBinary v;
        ERL_NIF_TERM t = make_bytes(env, NULL, 0);
        if (!enif_inspect_binary(env, t, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_is_binary(env, t) && v.size == 0) ? 1 : 0);
    }
    case 19: {                                 /* a full-range sub-binary equals the whole */
        ERL_NIF_TERM whole = make_bytes(env, "abcdef", 6);
        ERL_NIF_TERM s = enif_make_sub_binary(env, whole, 0, 6);
        ErlNifBinary v;
        if (!enif_inspect_binary(env, s, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            (v.size == 6 && memcmp(v.data, "abcdef", 6) == 0 &&
             enif_is_identical(whole, s)) ? 1 : 0);
    }
    case 20: {                                 /* inspect_iolist_as_binary on a plain binary */
        ErlNifBinary v;
        if (!enif_inspect_iolist_as_binary(env, make_bytes(env, "ab", 2), &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (v.size == 2 && memcmp(v.data, "ab", 2) == 0) ? 1 : 0);
    }
    case 21: {                                 /* get_string UTF-8 encodes a codepoint list */
        ERL_NIF_TERM arr[1];
        char buf[16];
        int r;
        arr[0] = enif_make_int(env, 0xE5);     /* U+00E5 encodes to the two bytes c3 a5 */
        r = enif_get_string(env, enif_make_list_from_array(env, arr, 1), buf, sizeof(buf),
                            ERL_NIF_UTF8);
        return enif_make_int(env,
            (r == 3 && (unsigned char) buf[0] == 0xC3 && (unsigned char) buf[1] == 0xA5)
                ? 1 : 0);
    }
    case 22: {                                 /* inspect_iolist_as_binary on the empty list */
        ErlNifBinary v;
        if (!enif_inspect_iolist_as_binary(env, make_nil(env), &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, v.size == 0 ? 1 : 0);
    }
    case 23: {                                 /* realloc_binary shrinking keeps the prefix */
        ErlNifBinary b;
        int ok;
        if (!enif_alloc_binary(4, &b))
            return enif_make_int(env, 0);
        memcpy(b.data, "abcd", 4);
        if (!enif_realloc_binary(&b, 2)) {
            enif_release_binary(&b);
            return enif_make_int(env, 0);
        }
        ok = (b.size == 2 && memcmp(b.data, "ab", 2) == 0);
        enif_release_binary(&b);
        return enif_make_int(env, ok ? 1 : 0);
    }
    case 24: {                                 /* a 0-byte owned binary round trips */
        ErlNifBinary b;
        ERL_NIF_TERM t;
        ErlNifBinary v;
        if (!enif_alloc_binary(0, &b))
            return enif_make_int(env, 0);
        t = enif_make_binary(env, &b);
        if (!enif_inspect_binary(env, t, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_is_binary(env, t) && v.size == 0) ? 1 : 0);
    }
    case 25: {                                 /* inspect_iolist_as_binary refuses a non-iolist */
        ErlNifBinary v;
        return enif_make_int(env,
            enif_inspect_iolist_as_binary(env, enif_make_atom(env, "nope"), &v) ? 0 : 1);
    }
    case 26: {                                 /* a 3-byte binary reports its term size */
        return enif_make_int(env,
            (int) (enif_term_size(make_bytes(env, "abc", 3)) / sizeof(ERL_NIF_TERM)));
    }

    /* --------------------------------------------------------------------------- maps */
    case 27: {                                 /* get_map_value refuses a missing key */
        ERL_NIF_TERM v = 0;
        ERL_NIF_TERM m;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "a"), enif_make_int(env, 1), &m))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            enif_get_map_value(env, m, enif_make_atom(env, "b"), &v) ? 0 : 1);
    }
    case 28: {                                 /* get_map_size refuses a non-map */
        size_t sz = 99;
        return enif_make_int(env,
            enif_get_map_size(env, enif_make_int(env, 1), &sz) ? 0 : 1);
    }
    case 29: {                                 /* get_map_value refuses a non-map */
        ERL_NIF_TERM v = 0;
        return enif_make_int(env,
            enif_get_map_value(env, enif_make_int(env, 1), enif_make_int(env, 1), &v) ? 0 : 1);
    }
    case 30: {                                 /* make_map_remove of an absent key is a no-op + 1 */
        ERL_NIF_TERM m1, m2;
        size_t sz = 99;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "a"), enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_remove(env, m1, enif_make_atom(env, "zz"), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m2, &sz))
            return enif_make_int(env, 0);
        return enif_make_int(env, (sz == 1 && enif_is_identical(m1, m2)) ? 1 : 0);
    }
    case 31: {                                 /* make_map_remove refuses a non-map */
        ERL_NIF_TERM m2;
        return enif_make_int(env,
            enif_make_map_remove(env, enif_make_int(env, 1), enif_make_int(env, 1), &m2) ? 0 : 1);
    }
    case 32: {                                 /* make_map_put refuses a non-map */
        ERL_NIF_TERM m2;
        return enif_make_int(env,
            enif_make_map_put(env, enif_make_int(env, 1), enif_make_int(env, 1),
                              enif_make_int(env, 1), &m2) ? 0 : 1);
    }
    case 33: {                                 /* make_map_from_arrays with zero pairs */
        ERL_NIF_TERM keys[1] = {0}, vals[1] = {0};
        ERL_NIF_TERM m;
        size_t sz = 99;
        if (!enif_make_map_from_arrays(env, keys, vals, 0, &m))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m, &sz))
            return enif_make_int(env, 0);
        return enif_make_int(env, sz == 0 ? 1 : 0);
    }
    case 34: {                                 /* the empty-map iterator is at head and tail */
        ErlNifMapIterator it;
        ERL_NIF_TERM k = 0, v = 0;
        int head, tail, stepped, pair;
        if (!enif_map_iterator_create(env, enif_make_new_map(env), &it,
                                      ERL_NIF_MAP_ITERATOR_FIRST))
            return enif_make_int(env, 0);
        head = enif_map_iterator_is_head(env, &it);
        tail = enif_map_iterator_is_tail(env, &it);
        stepped = enif_map_iterator_next(env, &it);
        pair = enif_map_iterator_get_pair(env, &it, &k, &v);
        enif_map_iterator_destroy(env, &it);
        return enif_make_int(env, (head && tail && !stepped && !pair) ? 1 : 0);
    }
    case 35: {                                 /* a FIRST iterator of a 3-map walks all pairs */
        ERL_NIF_TERM m1, m2, m3, k = 0, v = 0;
        int count = 0;
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
            count++;
            enif_map_iterator_next(env, &it);
        }
        enif_map_iterator_destroy(env, &it);
        return enif_make_int(env, count);
    }

    /* -------------------------------------------------- term type / compare / identity */
    case 36: {                                 /* term_type of a float */
        return enif_make_int(env,
            enif_term_type(env, enif_make_double(env, 1.5)) == ERL_NIF_TERM_TYPE_FLOAT ? 1 : 0);
    }
    case 37: {                                 /* term_type of the calling pid */
        ErlNifPid pid;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        return enif_make_int(env,
            enif_term_type(env, enif_make_pid(env, &pid)) == ERL_NIF_TERM_TYPE_PID ? 1 : 0);
    }
    case 38: {                                 /* term_type of [] is List */
        return enif_make_int(env,
            enif_term_type(env, make_nil(env)) == ERL_NIF_TERM_TYPE_LIST ? 1 : 0);
    }
    case 39: {                                 /* an atom orders before a tuple */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            enif_compare(enif_make_atom(env, "an_atom"),
                         enif_make_tuple_from_array(env, arr, 1)) == -1 ? 1 : 0);
    }
    case 40: {                                 /* a list orders before a bitstring */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            enif_compare(enif_make_list_from_array(env, arr, 1), make_bytes(env, "ab", 2)) == -1
                ? 1 : 0);
    }
    case 41: {                                 /* is_identical on structurally equal tuples */
        ERL_NIF_TERM a[2], b[2], c[2];
        int same, diff;
        a[0] = enif_make_int(env, 1); a[1] = enif_make_int(env, 2);
        b[0] = enif_make_int(env, 1); b[1] = enif_make_int(env, 2);
        c[0] = enif_make_int(env, 1); c[1] = enif_make_int(env, 3);
        same = enif_is_identical(enif_make_tuple_from_array(env, a, 2),
                                 enif_make_tuple_from_array(env, b, 2));
        diff = enif_is_identical(enif_make_tuple_from_array(env, a, 2),
                                 enif_make_tuple_from_array(env, c, 2));
        return enif_make_int(env, (same && !diff) ? 1 : 0);
    }
    case 42: {                                 /* is_identical on equal binaries */
        return enif_make_int(env,
            (enif_is_identical(make_bytes(env, "xy", 2), make_bytes(env, "xy", 2)) &&
             !enif_is_identical(make_bytes(env, "xy", 2), make_bytes(env, "xz", 2))) ? 1 : 0);
    }
    case 43: {                                 /* is_identical on equal lists */
        ERL_NIF_TERM a[2], b[2], c[2];
        int same, diff;
        a[0] = enif_make_int(env, 1); a[1] = enif_make_int(env, 2);
        b[0] = enif_make_int(env, 1); b[1] = enif_make_int(env, 2);
        c[0] = enif_make_int(env, 2); c[1] = enif_make_int(env, 1);
        same = enif_is_identical(enif_make_list_from_array(env, a, 2),
                                 enif_make_list_from_array(env, b, 2));
        diff = enif_is_identical(enif_make_list_from_array(env, a, 2),
                                 enif_make_list_from_array(env, c, 2));
        return enif_make_int(env, (same && !diff) ? 1 : 0);
    }
    case 44: {                                 /* is_number of a bignum */
        return enif_make_int(env,
            enif_is_number(env, enif_make_uint64(env, (ErlNifUInt64) 1 << 63)) ? 1 : 0);
    }
    case 45: {                                 /* is_atom([]) is false */
        return enif_make_int(env, enif_is_atom(env, make_nil(env)) ? 0 : 1);
    }

    /* ------------------------------------------------------------------------ strings */
    case 46: {                                 /* get_string over a list of char codes */
        ERL_NIF_TERM arr[2];
        char buf[16];
        int r;
        arr[0] = enif_make_int(env, 'h');
        arr[1] = enif_make_int(env, 'i');
        r = enif_get_string(env, enif_make_list_from_array(env, arr, 2), buf, sizeof(buf),
                            ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 3 && strcmp(buf, "hi") == 0) ? 1 : 0);
    }
    case 47: {                                 /* the empty string round trips */
        char buf[16];
        int r = enif_get_string(env, enif_make_string(env, "", ERL_NIF_LATIN1), buf, sizeof(buf),
                                ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && buf[0] == '\0') ? 1 : 0);
    }
    case 48: {                                 /* get_string_length of [] is 0 */
        unsigned n = 99;
        int r = enif_get_string_length(env, make_nil(env), &n, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && n == 0) ? 1 : 0);
    }
    case 49: {                                 /* get_string refuses an improper list */
        ERL_NIF_TERM cell = enif_make_list_cell(env, enif_make_int(env, 'a'),
                                                enif_make_int(env, 2));
        char buf[16];
        return enif_make_int(env,
            enif_get_string(env, cell, buf, sizeof(buf), ERL_NIF_LATIN1) ? 0 : 1);
    }
    case 50: {                                 /* get_string refuses a non-byte element */
        ERL_NIF_TERM arr[1];
        char buf[16];
        arr[0] = enif_make_int(env, 256);
        return enif_make_int(env,
            enif_get_string(env, enif_make_list_from_array(env, arr, 1), buf, sizeof(buf),
                            ERL_NIF_LATIN1) ? 0 : 1);
    }
    case 51: {                                 /* get_atom refuses a too-small buffer */
        char buf[2];
        return enif_make_int(env,
            enif_get_atom(env, enif_make_atom(env, "ok"), buf, sizeof(buf), ERL_NIF_LATIN1)
                ? 0 : 1);
    }
    case 52: {                                 /* get_atom_length of an existing name */
        unsigned n = 0;
        int r = enif_get_atom_length(env, enif_make_atom(env, "ok"), &n, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && n == 2) ? 1 : 0);
    }

    /* ---------------------------------------------------------------------- term size */
    case 53: {                                 /* term_size of a 2-tuple is 3 words */
        ERL_NIF_TERM arr[2];
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        return enif_make_int(env,
            (int) (enif_term_size(enif_make_tuple_from_array(env, arr, 2))
                   / sizeof(ERL_NIF_TERM)));
    }
    case 54: {                                 /* term_size of [] is 0 */
        return enif_make_int(env,
            (int) (enif_term_size(make_nil(env)) / sizeof(ERL_NIF_TERM)));
    }
    case 55: {                                 /* term_size of an atom is 0 */
        return enif_make_int(env,
            (int) (enif_term_size(enif_make_atom(env, "an_atom")) / sizeof(ERL_NIF_TERM)));
    }

    /* --------------------------------------------------------------------------- hash */
    case 56: {                                 /* phash2 of a binary: salt-independent, < 2^27 */
        ERL_NIF_TERM t = make_bytes(env, "hello", 5);
        ErlNifUInt64 h0 = enif_hash(ERL_NIF_PHASH2, t, 0);
        ErlNifUInt64 h1 = enif_hash(ERL_NIF_PHASH2, t, 424242);
        return enif_make_int(env,
            (h0 == h1 && h0 < ((ErlNifUInt64) 1 << 27)) ? 1 : 0);
    }

    /* --------------------------------------------------------------------- exceptions */
    case 57: {                                 /* a tuple raise reason reaches Erlang */
        ERL_NIF_TERM arr[2];
        arr[0] = enif_make_atom(env, "reason");
        arr[1] = enif_make_int(env, 7);
        return enif_raise_exception(env, enif_make_tuple_from_array(env, arr, 2));
    }
    case 58: {                                 /* an integer raise reason reaches Erlang */
        return enif_raise_exception(env, enif_make_int(env, 42));
    }
    case 59: {                                 /* the process env has no pending exception */
        ERL_NIF_TERM reason = 0;
        return enif_make_int(env, enif_has_pending_exception(env, &reason) == 0 ? 1 : 0);
    }

    /* --------------------------------------------------------------------------- time */
    case 60: {                                 /* convert_time_unit 1 s -> ns */
        ErlNifTime ns = enif_convert_time_unit((ErlNifTime) 1, ERL_NIF_SEC, ERL_NIF_NSEC);
        return enif_make_int(env, ns == 1000000000 ? 1 : 0);
    }
    case 61: {                                 /* convert_time_unit 1 s -> ms */
        ErlNifTime ms = enif_convert_time_unit((ErlNifTime) 1, ERL_NIF_SEC, ERL_NIF_MSEC);
        return enif_make_int(env, ms == 1000 ? 1 : 0);
    }
    case 62: {                                 /* monotonic_time never runs backwards */
        ErlNifTime a = enif_monotonic_time(ERL_NIF_MSEC);
        ErlNifTime b = enif_monotonic_time(ERL_NIF_MSEC);
        return enif_make_int(env, b >= a ? 1 : 0);
    }

    /* ------------------------------------ the remaining enif_term_type lattice points */
    case 63: {                                 /* term_type of a bignum is Integer */
        ERL_NIF_TERM t = enif_make_uint64(env, (ErlNifUInt64) 1 << 63);
        return enif_make_int(env,
            enif_term_type(env, t) == ERL_NIF_TERM_TYPE_INTEGER ? 1 : 0);
    }
    case 64: {                                 /* term_type of a cons cell is List */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 7);
        return enif_make_int(env,
            enif_term_type(env, enif_make_list_from_array(env, arr, 1)) == ERL_NIF_TERM_TYPE_LIST
                ? 1 : 0);
    }
    case 65: {                                 /* term_type of a 2-entry map is Map */
        ERL_NIF_TERM keys[2], vals[2], m;
        keys[0] = enif_make_atom(env, "a"); vals[0] = enif_make_int(env, 1);
        keys[1] = enif_make_atom(env, "b"); vals[1] = enif_make_int(env, 2);
        if (!enif_make_map_from_arrays(env, keys, vals, 2, &m))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            enif_term_type(env, m) == ERL_NIF_TERM_TYPE_MAP ? 1 : 0);
    }
    case 66: {                                 /* term_type of a 2-tuple is Tuple */
        ERL_NIF_TERM arr[2];
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        return enif_make_int(env,
            enif_term_type(env, enif_make_tuple_from_array(env, arr, 2))
                == ERL_NIF_TERM_TYPE_TUPLE ? 1 : 0);
    }

    /* --------------------------------------------- more enif_compare order lattice points */
    case 67: {                                 /* 1 ranks equal to 1.0 (arithmetic CMP) */
        return enif_make_int(env,
            enif_compare(enif_make_int(env, 1), enif_make_double(env, 1.0)) == 0 ? 1 : 0);
    }
    case 68: {                                 /* an atom orders before another by name */
        return enif_make_int(env,
            enif_compare(enif_make_atom(env, "alpha"), enif_make_atom(env, "beta")) == -1
                ? 1 : 0);
    }
    case 69: {                                 /* a tuple orders before a map */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            enif_compare(enif_make_tuple_from_array(env, arr, 1), enif_make_new_map(env)) == -1
                ? 1 : 0);
    }

    /* --------------------------------------------------------------- more predicates */
    case 70: {                                 /* is_pid accepts the pid from enif_self */
        ErlNifPid pid;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_pid(env, enif_make_pid(env, &pid)) ? 1 : 0);
    }
    case 71: {                                 /* is_map accepts a map that holds a pair */
        ERL_NIF_TERM m;
        if (!enif_make_map_put(env, enif_make_new_map(env), enif_make_atom(env, "k"),
                               enif_make_int(env, 1), &m))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_map(env, m) ? 1 : 0);
    }

    /* ----------------------------------------------- the other integer boundary pairs */
    case 72: {                                 /* UINT_MAX survives make_uint/get_uint */
        ERL_NIF_TERM t = enif_make_uint(env, (unsigned) UINT_MAX);
        unsigned v = 0;
        return enif_make_int(env,
            (enif_get_uint(env, t, &v) && v == (unsigned) UINT_MAX) ? 1 : 0);
    }
    case 73: {                                 /* LONG_MIN survives make_long/get_long */
        ERL_NIF_TERM t = enif_make_long(env, (long) LONG_MIN);
        long v = 0;
        return enif_make_int(env, (enif_get_long(env, t, &v) && v == LONG_MIN) ? 1 : 0);
    }
    case 74: {                                 /* INT_MIN survives make_int/get_int */
        ERL_NIF_TERM t = enif_make_int(env, (int) INT_MIN);
        int v = 0;
        return enif_make_int(env, (enif_get_int(env, t, &v) && v == INT_MIN) ? 1 : 0);
    }
    case 75: {                                 /* ULONG_MAX survives make_ulong/get_ulong */
        ERL_NIF_TERM t = enif_make_ulong(env, (unsigned long) ULONG_MAX);
        unsigned long v = 0;
        return enif_make_int(env,
            (enif_get_ulong(env, t, &v) && v == ULONG_MAX) ? 1 : 0);
    }
    case 76: {                                 /* get_int refuses a value below INT_MIN */
        ERL_NIF_TERM t = enif_make_int64(env, (ErlNifSInt64) INT_MIN - 1);
        int v = 0;
        return enif_make_int(env, enif_get_int(env, t, &v) ? 0 : 1);
    }

    /* ------------------------------------------------------- more map build/lookup edges */
    case 77: {                                 /* make_map_from_arrays with two distinct pairs */
        ERL_NIF_TERM keys[2], vals[2], m, v = 0;
        size_t sz = 0;
        keys[0] = enif_make_atom(env, "a"); vals[0] = enif_make_int(env, 1);
        keys[1] = enif_make_atom(env, "b"); vals[1] = enif_make_int(env, 2);
        if (!enif_make_map_from_arrays(env, keys, vals, 2, &m))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m, &sz) || sz != 2)
            return enif_make_int(env, 0);
        if (!enif_get_map_value(env, m, keys[1], &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_identical(v, vals[1]) ? 1 : 0);
    }
    case 78: {                                 /* putting an existing key updates in place */
        ERL_NIF_TERM m1, m2, v = 0;
        size_t sz = 0;
        int iv = 0;
        if (!enif_make_map_put(env, enif_make_new_map(env), enif_make_atom(env, "k"),
                               enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_put(env, m1, enif_make_atom(env, "k"), enif_make_int(env, 2), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m2, &sz) || sz != 1)
            return enif_make_int(env, 0);
        if (!enif_get_map_value(env, m2, enif_make_atom(env, "k"), &v))
            return enif_make_int(env, 0);
        if (!enif_get_int(env, v, &iv) || iv != 2)
            return enif_make_int(env, 0);
        return enif_make_int(env, 1);
    }

    /* -------------------------------------------------------------- string / atom edges */
    case 79: {                                 /* an exactly-fitting LATIN1 string buffer */
        ERL_NIF_TERM arr[3];
        char buf[4];
        int r;
        arr[0] = enif_make_int(env, 'a');
        arr[1] = enif_make_int(env, 'b');
        arr[2] = enif_make_int(env, 'c');
        r = enif_get_string(env, enif_make_list_from_array(env, arr, 3), buf, sizeof(buf),
                            ERL_NIF_LATIN1);
        if (r != 4 || memcmp(buf, "abc", 4) != 0)
            return enif_make_int(env, -100);
        return enif_make_int(env, r);
    }
    case 80: {                                 /* get_string refuses a binary subject */
        char buf[8];
        return enif_make_int(env,
            enif_get_string(env, make_bytes(env, "ab", 2), buf, sizeof(buf), ERL_NIF_LATIN1)
                ? 0 : 1);
    }
    case 81: {                                 /* a UTF-8 atom name reads back under UTF8 only */
        static const char utf8_name[3] = {(char) 0xE2, (char) 0x82, (char) 0xAC};  /* U+20AC */
        ERL_NIF_TERM atom = 0;
        char buf[8];
        int r, latin1;
        if (!enif_make_new_atom_len(env, utf8_name, sizeof(utf8_name), &atom, ERL_NIF_UTF8))
            return enif_make_int(env, 0);
        r = enif_get_atom(env, atom, buf, sizeof(buf), ERL_NIF_UTF8);
        latin1 = enif_get_atom(env, atom, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 4 && !latin1) ? 1 : 0);
    }
    case 82: {                                 /* get_atom_length counts the UTF-8 bytes (3) */
        static const char utf8_name[3] = {(char) 0xE2, (char) 0x82, (char) 0xAC};  /* U+20AC */
        ERL_NIF_TERM atom = 0;
        unsigned n = 0;
        if (!enif_make_new_atom_len(env, utf8_name, sizeof(utf8_name), &atom, ERL_NIF_UTF8))
            return enif_make_int(env, 0);
        if (!enif_get_atom_length(env, atom, &n, ERL_NIF_UTF8))
            return enif_make_int(env, 0);
        return enif_make_int(env, (n == 3) ? 1 : 0);
    }

    /* ---------------------------------------------------------------- time unit pairs */
    case 83: {                                 /* convert_time_unit 1e9 ns -> 1e6 us */
        ErlNifTime us =
            enif_convert_time_unit((ErlNifTime) 1000000000, ERL_NIF_NSEC, ERL_NIF_USEC);
        return enif_make_int(env, us == 1000000 ? 1 : 0);
    }

    /* ------------------------------------------- atom / string constructors with a length (84..86) */
    case 84: {                                 /* make_atom_len interns the same atom as make_atom */
        return enif_make_int(env,
            enif_is_identical(enif_make_atom_len(env, "hello", 5),
                              enif_make_atom(env, "hello")) ? 1 : 0);
    }
    case 85: {                                 /* make_existing_atom_len refuses an absent name */
        const char absent[] = "nif_extra_absent_atom_zz";
        ERL_NIF_TERM atom = 0;
        return enif_make_int(env,
            enif_make_existing_atom_len(env, absent, sizeof(absent) - 1, &atom, ERL_NIF_LATIN1)
                ? 0 : 1);
    }
    case 86: {                                 /* make_string_len writes only the first 3 bytes */
        char buf[8];
        int r;
        memset(buf, 0x7F, sizeof(buf));
        r = enif_get_string(env, enif_make_string_len(env, "hello", 3, ERL_NIF_LATIN1),
                            buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 4 && memcmp(buf, "hel", 4) == 0) ? 1 : 0);
    }

    /* -------------------------------------------------------- env copy across environments (87) */
    case 87: {                                 /* make_copy into a fresh env keeps the shape */
        ERL_NIF_TERM arr[2];
        ERL_NIF_TERM copied;
        ErlNifEnv* dst = enif_alloc_env();
        int arity = -1, a0 = 0, a1 = 0, ok;
        const ERL_NIF_TERM* got = NULL;
        if (dst == NULL)
            return enif_make_int(env, 0);
        arr[0] = enif_make_int(env, 7);
        arr[1] = enif_make_int(env, 8);
        copied = enif_make_copy(dst, enif_make_tuple_from_array(env, arr, 2));
        ok = enif_get_tuple(dst, copied, &arity, &got);
        if (ok)
            ok = enif_get_int(dst, got[0], &a0) && enif_get_int(dst, got[1], &a1);
        enif_free_env(dst);
        return enif_make_int(env, (ok && arity == 2 && a0 == 7 && a1 == 8) ? 1 : 0);
    }

    /* ---------------------------------------------------- local pid / port accessors (88..90) */
    case 88: {                                 /* get_local_pid accepts the calling pid */
        ErlNifPid pid, back;
        ERL_NIF_TERM self;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        self = enif_make_pid(env, &pid);
        if (!enif_get_local_pid(env, self, &back))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_identical(self, enif_make_pid(env, &back)) ? 1 : 0);
    }
    case 89: {                                 /* get_local_pid refuses a non-pid */
        ErlNifPid pid;
        return enif_make_int(env,
            enif_get_local_pid(env, enif_make_atom(env, "not_a_pid"), &pid) ? 0 : 1);
    }
    case 90: {                                 /* get_local_port refuses a non-port */
        ErlNifPort port;
        return enif_make_int(env,
            enif_get_local_port(env, enif_make_int(env, 1), &port) ? 0 : 1);
    }

    /* ------------------------------------------------------- negative predicate lattice (91..92) */
    case 91: {                                 /* is_ref/is_fun/is_port all refuse a 2-tuple */
        ERL_NIF_TERM arr[2];
        ERL_NIF_TERM t;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        t = enif_make_tuple_from_array(env, arr, 2);
        return enif_make_int(env,
            (!enif_is_ref(env, t) && !enif_is_fun(env, t) && !enif_is_port(env, t)) ? 1 : 0);
    }
    case 92: {                                 /* is_exception is false for an ordinary term */
        return enif_make_int(env,
            enif_is_exception(env, enif_make_int(env, 1)) ? 0 : 1);
    }

    /* --------------------------------------------------------------- map update, not put (93..94) */
    case 93: {                                 /* make_map_update refuses an absent key */
        ERL_NIF_TERM out;
        return enif_make_int(env,
            enif_make_map_update(env, enif_make_new_map(env), enif_make_atom(env, "k"),
                                 enif_make_int(env, 1), &out) ? 0 : 1);
    }
    case 94: {                                 /* make_map_update replaces an existing value */
        ERL_NIF_TERM m1, m2, v = 0;
        size_t sz = 0;
        int iv = 0;
        if (!enif_make_map_put(env, enif_make_new_map(env), enif_make_atom(env, "k"),
                               enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_update(env, m1, enif_make_atom(env, "k"), enif_make_int(env, 2), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m2, &sz) || sz != 1)
            return enif_make_int(env, 0);
        if (!enif_get_map_value(env, m2, enif_make_atom(env, "k"), &v))
            return enif_make_int(env, 0);
        if (!enif_get_int(env, v, &iv) || iv != 2)
            return enif_make_int(env, 0);
        return enif_make_int(env, 1);
    }

    /* ------------------------------------------------------------------------ tid equality (95) */
    case 95: {                                 /* the calling thread's tid equals itself */
        ErlNifTid tid = enif_thread_self();
        return enif_make_int(env, enif_equal_tids(tid, tid) ? 1 : 0);
    }

    default:
        break;
    }
    return enif_make_int(env, -1);
}

/* The term-driven entry: observe a term the module built in Erlang. The NIF C API has no
 * bitstring constructor, so a non-byte-aligned bitstring (and a window of one) can only be built
 * on the Erlang side; the same route carries the shapes the constructors cannot express either
 * (an improper list). Each arm returns a plain integer, so every golden stays VM-stable. */
static ERL_NIF_TERM probe_term(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int sel = 0;
    ERL_NIF_TERM term;

    if (argc != 2 || !enif_get_int(env, argv[0], &sel))
        return enif_make_badarg(env);
    term = argv[1];

    switch (sel) {
    /* ------------------------------------------- enif_inspect_iolist_as_binary shape rules */
    case 101:                                  /* a BARE byte integer is not an iolist */
        return enif_make_int(env, iolist_shape(env, term, NULL) == -1 ? 1 : 0);
    case 102:                                  /* an improper-list tail [1|2] */
        return enif_make_int(env, iolist_shape(env, term, NULL) == -1 ? 1 : 0);
    case 103:                                  /* a non-byte integer head [256] */
        return enif_make_int(env, iolist_shape(env, term, NULL) == -1 ? 1 : 0);
    case 104: {                                /* a bit-sized element [<<1:7>>] */
        return enif_make_int(env, iolist_shape(env, term, NULL) == -1 ? 1 : 0);
    }
    case 105: case 108: {                      /* [] and [[]]: the accepted empty iolist, size 0 */
        return enif_make_int(env, iolist_is_empty(env, term));
    }
    case 106: {                                /* [1,[2,<<3>>]] flattens to <<1,2,3>> */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        /* 1*1 + 2*2 + 3*3 = 14 */
        return enif_make_int(env, (n == 3 && sum == 14) ? 1 : 0);
    }
    case 107: {                                /* [<<1:8>>] flattens to the single byte 1 */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        return enif_make_int(env, (n == 1 && sum == 1) ? 1 : 0);
    }
    case 109: {                                /* [1|<<2>>] flattens to <<1,2>> */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        /* 1*1 + 2*2 = 5 */
        return enif_make_int(env, (n == 2 && sum == 5) ? 1 : 0);
    }
    /* ---------------------------------------------------------------- enif_inspect_binary */
    case 110:                                  /* a byte-sized bit-offset window reports 1 + bytes */
        return enif_make_int64(env, (ErlNifSInt64) binary_view(env, term));
    case 111:                                  /* a non-byte-sized bitstring is still refused */
        return enif_make_int(env, binary_view(env, term) == -1 ? 1 : 0);

    /* --------------------------------------------------------------- enif_make_sub_binary */
    case 120:                                  /* a bit-offset window, byte pos 0, 8 bytes */
        return enif_make_int64(env, (ErlNifSInt64) sub_binary_view(env, term, 0, 8));
    case 121:                                  /* a bit-offset window, byte pos 4, 8 bytes */
        return enif_make_int64(env, (ErlNifSInt64) sub_binary_view(env, term, 4, 8));
    case 122: case 123: {                      /* a zero-size window is the empty binary */
        size_t pos = (sel == 122) ? 3 : 7;      /* 7 is one past the last byte of <<"abcdef">> */
        ERL_NIF_TERM s = enif_make_sub_binary(env, term, pos, 0);
        ErlNifBinary v;
        if (!enif_inspect_binary(env, s, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (v.size == 0 && enif_is_binary(env, s)) ? 1 : 0);
    }

    /* ------------------------------------------------- enif_get_string under ERL_NIF_UTF8 */
    case 130: {                                /* the ASCII prefix + one whole char fit exactly */
        static const unsigned char want[5] = {0x61, 0xE2, 0x82, 0xAC, 0x00};
        char buf[8];
        int r;
        memset(buf, 0x7F, sizeof(buf));
        r = enif_get_string(env, term, buf, 5, ERL_NIF_UTF8);
        if (r != 5 || memcmp(buf, want, sizeof(want)) != 0)
            return enif_make_int(env, -100);
        return enif_make_int(env, r);
    }
    case 131: {                                /* one byte too small: the char is dropped whole */
        char buf[8];
        int r;
        memset(buf, 0x7F, sizeof(buf));
        r = enif_get_string(env, term, buf, 4, ERL_NIF_UTF8);
        if (r != -2 || (unsigned char) buf[0] != 0x61 || buf[1] != '\0')
            return enif_make_int(env, -100);
        return enif_make_int(env, r);
    }
    case 132: {                                /* a lone multibyte char that cannot fit at all */
        char buf[8];
        int r;
        memset(buf, 0x7F, sizeof(buf));
        r = enif_get_string(env, term, buf, 3, ERL_NIF_UTF8);
        if (r != -1 || buf[0] != '\0')
            return enif_make_int(env, -100);
        return enif_make_int(env, r);
    }

    /* ------------------------------------- the widened binary / iolist shape block (133..143) */
    case 133:                                  /* a non-byte-aligned bitstring stays refused */
        return enif_make_int(env, binary_view(env, term) == -1 ? 1 : 0);
    case 134: {                                /* the empty binary is accepted with size 0 */
        ErlNifBinary v;
        if (!enif_inspect_binary(env, term, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (v.size == 0 && enif_is_binary(env, term)) ? 1 : 0);
    }
    case 135: {                                /* a binary orders by byte value */
        char higher[3] = {1, 2, 4};
        return enif_make_int(env,
            enif_compare(term, make_bytes(env, higher, 3)) == -1 ? 1 : 0);
    }
    case 136:                                  /* a binary is identical to, and weighs equal to, itself */
        return enif_make_int(env,
            (enif_is_identical(term, term) && enif_compare(term, term) == 0) ? 1 : 0);
    case 137: {                                /* [[1,2],[3,4]] flattens to <<1,2,3,4>> */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        /* 1*1 + 2*2 + 3*3 + 4*4 = 30 */
        return enif_make_int(env, (n == 4 && sum == 30) ? 1 : 0);
    }
    case 138: {                                /* [<<"ab">>,99] flattens to <<97,98,99>> */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        /* 1*97 + 2*98 + 3*99 = 590 */
        return enif_make_int(env, (n == 3 && sum == 590) ? 1 : 0);
    }
    case 139: {                                /* the zero byte is an accepted iolist element */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        return enif_make_int(env, (n == 1 && sum == 0) ? 1 : 0);
    }
    case 140: {                                /* 255 is the highest accepted byte */
        unsigned long long sum = 0;
        int n = iolist_shape(env, term, &sum);
        return enif_make_int(env, (n == 1 && sum == 255) ? 1 : 0);
    }
    case 141:                                  /* [<<>>] is the accepted empty iolist */
        return enif_make_int(env, iolist_is_empty(env, term));
    case 142:                                  /* [[],[]] is the accepted empty iolist */
        return enif_make_int(env, iolist_is_empty(env, term));
    case 143: {                                /* enif_make_sub_binary windows <<"abcd">> */
        size_t pos = 1, size = 2;
        return enif_make_int64(env, (ErlNifSInt64) sub_binary_view(env, term, pos, size));
    }

    default:
        break;
    }
    return enif_make_int(env, -1);
}

static ErlNifFunc nif_extra_funcs[] = {
    {"probe", 1, probe, 0},
    {"probe_term", 2, probe_term, 0}
};
/* `ERL_NIF_INIT` seeds ErlNifEntry.name with the stringified module name (`nif_extra`), so this
   library may only be loaded as `nif_extra`. */
ERL_NIF_INIT(nif_extra, nif_extra_funcs, load, NULL, NULL, NULL)
"#;

/// The real library every case loads.
const NIF_EXTRA: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_extra",
    c_source: NIF_EXTRA_C,
};

/// Build one module-run case over [`NIF_EXTRA_MODULE_SOURCE`].
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
            source: NIF_EXTRA_MODULE_SOURCE,
            module: "nif_extra",
            function: "classify",
            arity: 1,
            args: selectors,
            nif: Some(NIF_EXTRA),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The widened A.3 group: a second `enif_*` selector set, one per C ABI behaviour.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- integer boundaries ------------------------------------------------------------
        case(
            "nif_extra/int_get_int64_rejects_positive_2_63",
            "enif_get_int64 refuses the 2^63 written by enif_make_uint64 (enif_get_uint64 takes it)",
            &[1],
            "1",
        ),
        case(
            "nif_extra/int_get_uint64_rejects_negative",
            "enif_get_uint64 refuses a negative integer",
            &[2],
            "1",
        ),
        case(
            "nif_extra/int_get_uint_rejects_above_uint_max",
            "enif_get_uint refuses a value above UINT_MAX",
            &[3],
            "1",
        ),
        case(
            "nif_extra/int_int64_min_roundtrip",
            "INT64_MIN survives an enif_make_int64/enif_get_int64 round trip",
            &[4],
            "1",
        ),
        case(
            "nif_extra/float_get_double_large_magnitude",
            "enif_make_double/enif_get_double round trip a 1.0e308 magnitude",
            &[5],
            "1",
        ),
        case("nif_extra/int_get_uint_rejects_float", "enif_get_uint refuses a float", &[6], "1"),
        case("nif_extra/int_get_long_rejects_atom", "enif_get_long refuses a non-number", &[7], "1"),
        case(
            "nif_extra/int_get_ulong_rejects_negative",
            "enif_get_ulong refuses a negative integer",
            &[8],
            "1",
        ),
        // --- lists ------------------------------------------------------------------------
        case(
            "nif_extra/list_get_list_length_rejects_improper",
            "enif_get_list_length refuses an improper list",
            &[9],
            "1",
        ),
        case(
            "nif_extra/list_get_list_cell_rejects_nil",
            "enif_get_list_cell refuses the empty list",
            &[10],
            "1",
        ),
        case(
            "nif_extra/list_tail_of_singleton_is_nil",
            "the tail of a 1-element list is the empty list",
            &[11],
            "1",
        ),
        case(
            "nif_extra/list_is_list_nil_true_empty_list_cons_false",
            "enif_is_list accepts [] while enif_is_empty_list refuses a cons cell",
            &[12],
            "1",
        ),
        case(
            "nif_extra/list_make_reverse_list_empty",
            "enif_make_reverse_list of [] yields []",
            &[13],
            "1",
        ),
        case(
            "nif_extra/list_get_list_length_three",
            "enif_get_list_length reports 3 for a 3-element list",
            &[14],
            "1",
        ),
        // --- tuples -----------------------------------------------------------------------
        case(
            "nif_extra/tuple_one_element",
            "a 1-tuple: enif_get_tuple reports arity 1 and the element",
            &[15],
            "1",
        ),
        case(
            "nif_extra/tuple_empty_is_tuple_arity_zero",
            "enif_is_tuple accepts {} and enif_get_tuple reports arity 0",
            &[16],
            "1",
        ),
        case("nif_extra/tuple_get_tuple_arity_two", "enif_get_tuple reports the arity (2)", &[17], "2"),
        // --- binaries ---------------------------------------------------------------------
        case(
            "nif_extra/binary_inspect_empty",
            "enif_inspect_binary accepts the empty binary with size 0",
            &[18],
            "1",
        ),
        case(
            "nif_extra/binary_make_sub_binary_full_range",
            "a full-range enif_make_sub_binary equals the whole binary",
            &[19],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_iolist_as_binary_flat",
            "enif_inspect_iolist_as_binary on a plain binary",
            &[20],
            "1",
        ),
        case(
            "nif_extra/string_get_utf8_from_codepoint_list",
            "enif_get_string under ERL_NIF_UTF8 encodes a codepoint list (U+00E5 -> c3 a5)",
            &[21],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_iolist_empty_list",
            "enif_inspect_iolist_as_binary of [] yields a 0-byte view",
            &[22],
            "1",
        ),
        case(
            "nif_extra/binary_realloc_binary_shrink",
            "enif_realloc_binary shrinks and keeps the prefix",
            &[23],
            "1",
        ),
        case(
            "nif_extra/binary_zero_byte_owned_roundtrip",
            "a 0-byte enif_alloc_binary/enif_make_binary round trip",
            &[24],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_iolist_rejects_atom",
            "enif_inspect_iolist_as_binary refuses a non-iolist",
            &[25],
            "1",
        ),
        case(
            "nif_extra/term_size_three_byte_binary",
            "enif_term_size of a 3-byte binary (in words)",
            &[26],
            "3",
        ),
        // --- maps -------------------------------------------------------------------------
        case("nif_extra/map_get_map_value_missing_key", "enif_get_map_value refuses a missing key", &[27], "1"),
        case("nif_extra/map_get_map_size_rejects_non_map", "enif_get_map_size refuses a non-map", &[28], "1"),
        case("nif_extra/map_get_map_value_rejects_non_map", "enif_get_map_value refuses a non-map", &[29], "1"),
        case(
            "nif_extra/map_make_map_remove_absent_key",
            "enif_make_map_remove of an absent key answers 1 and leaves the map unchanged",
            &[30],
            "1",
        ),
        case("nif_extra/map_make_map_remove_rejects_non_map", "enif_make_map_remove refuses a non-map", &[31], "1"),
        case("nif_extra/map_make_map_put_rejects_non_map", "enif_make_map_put refuses a non-map", &[32], "1"),
        case(
            "nif_extra/map_make_map_from_arrays_zero",
            "enif_make_map_from_arrays with zero pairs yields an empty map",
            &[33],
            "1",
        ),
        case(
            "nif_extra/map_iterator_empty_map_head_tail",
            "an empty-map iterator is at head and tail and yields no pair",
            &[34],
            "1",
        ),
        case(
            "nif_extra/map_iterator_first_walks_three",
            "a FIRST iterator walks all three pairs of a 3-entry map",
            &[35],
            "3",
        ),
        // --- term type / compare / identity -----------------------------------------------
        case("nif_extra/term_type_float", "enif_term_type reports Float for a double", &[36], "1"),
        case("nif_extra/term_type_pid", "enif_term_type reports Pid for the calling process", &[37], "1"),
        case("nif_extra/term_type_nil_is_list", "enif_term_type reports List for []", &[38], "1"),
        case("nif_extra/compare_atom_before_tuple", "enif_compare orders an atom before a tuple", &[39], "1"),
        case("nif_extra/compare_list_before_bitstring", "enif_compare orders a list before a bitstring", &[40], "1"),
        case(
            "nif_extra/identity_equal_tuples",
            "enif_is_identical holds for two structurally equal tuples and differs otherwise",
            &[41],
            "1",
        ),
        case(
            "nif_extra/identity_equal_binaries",
            "enif_is_identical holds for two binaries with the same bytes",
            &[42],
            "1",
        ),
        case("nif_extra/identity_equal_lists", "enif_is_identical holds for two structurally equal lists", &[43], "1"),
        case("nif_extra/pred_is_number_bignum", "enif_is_number accepts a bignum term", &[44], "1"),
        case("nif_extra/pred_is_atom_nil_false", "enif_is_atom is false for []", &[45], "1"),
        // --- strings / atoms --------------------------------------------------------------
        case("nif_extra/string_get_from_char_list", "enif_get_string reads a list of char codes", &[46], "1"),
        case("nif_extra/string_empty_roundtrip", "enif_make_string(\"\") round trips through enif_get_string", &[47], "1"),
        case("nif_extra/string_get_length_nil", "enif_get_string_length of [] reports 0", &[48], "1"),
        case("nif_extra/string_get_rejects_improper", "enif_get_string refuses an improper list", &[49], "1"),
        case("nif_extra/string_get_rejects_non_byte", "enif_get_string refuses a non-byte element", &[50], "1"),
        case("nif_extra/atom_get_atom_small_buffer", "enif_get_atom refuses a too-small buffer", &[51], "1"),
        case("nif_extra/atom_get_atom_length", "enif_get_atom_length reports the byte count (2)", &[52], "1"),
        // --- term size / hash -------------------------------------------------------------
        case("nif_extra/term_size_two_tuple", "enif_term_size of a 2-tuple is three words", &[53], "3"),
        case("nif_extra/term_size_nil", "enif_term_size of [] is zero words", &[54], "0"),
        case("nif_extra/term_size_atom", "enif_term_size of an atom is zero words", &[55], "0"),
        case(
            "nif_extra/hash_phash2_binary_salt_independent",
            "enif_hash(ERL_NIF_PHASH2, <<binary>>) ignores the salt and stays below 2^27",
            &[56],
            "1",
        ),
        // --- exceptions -------------------------------------------------------------------
        case(
            "nif_extra/exception_tuple_reason",
            "enif_raise_exception lifts a tuple reason into Erlang as error:Reason",
            &[57],
            "82",
        ),
        case(
            "nif_extra/exception_integer_reason",
            "enif_raise_exception lifts an integer reason into Erlang as error:Reason",
            &[58],
            "82",
        ),
        case(
            "nif_extra/exception_no_pending_on_process_env",
            "enif_has_pending_exception is 0 on the process env with no raise",
            &[59],
            "1",
        ),
        // --- time -------------------------------------------------------------------------
        case("nif_extra/time_convert_sec_to_nsec", "enif_convert_time_unit maps 1 s to 1e9 ns", &[60], "1"),
        case("nif_extra/time_convert_sec_to_msec", "enif_convert_time_unit maps 1 s to 1000 ms", &[61], "1"),
        case("nif_extra/time_monotonic_non_decreasing", "enif_monotonic_time never runs backwards", &[62], "1"),
        // --- enif_term_type: the remaining lattice points -----------------------------------
        case(
            "nif_extra/term_type_bignum_is_integer",
            "enif_term_type reports Integer for a bignum (2^63)",
            &[63],
            "1",
        ),
        case(
            "nif_extra/term_type_cons_cell_is_list",
            "enif_term_type reports List for a cons cell",
            &[64],
            "1",
        ),
        case(
            "nif_extra/term_type_two_entry_map_is_map",
            "enif_term_type reports Map for a map built by enif_make_map_from_arrays",
            &[65],
            "1",
        ),
        case(
            "nif_extra/term_type_two_tuple_is_tuple",
            "enif_term_type reports Tuple for a 2-tuple",
            &[66],
            "1",
        ),
        // --- enif_compare: more order-lattice points ----------------------------------------
        case(
            "nif_extra/compare_int_one_equals_float_one",
            "enif_compare(1, 1.0) ranks the integer equal to the float",
            &[67],
            "1",
        ),
        case(
            "nif_extra/compare_alpha_orders_before_beta",
            "enif_compare('alpha','beta') orders two atom names (-1)",
            &[68],
            "1",
        ),
        case(
            "nif_extra/compare_tuple_before_map",
            "enif_compare orders a tuple before a map (-1)",
            &[69],
            "1",
        ),
        // --- more predicates ----------------------------------------------------------------
        case(
            "nif_extra/pred_is_pid_of_self",
            "enif_is_pid accepts the pid term written by enif_make_pid(enif_self)",
            &[70],
            "1",
        ),
        case(
            "nif_extra/pred_is_map_with_one_pair",
            "enif_is_map accepts a map that holds one pair",
            &[71],
            "1",
        ),
        // --- the remaining integer boundary pairs -------------------------------------------
        case(
            "nif_extra/int_get_uint_uint_max_roundtrip",
            "UINT_MAX survives an enif_make_uint/enif_get_uint round trip",
            &[72],
            "1",
        ),
        case(
            "nif_extra/int_get_long_long_min_roundtrip",
            "LONG_MIN survives an enif_make_long/enif_get_long round trip",
            &[73],
            "1",
        ),
        case(
            "nif_extra/int_get_int_int_min_roundtrip",
            "INT_MIN survives an enif_make_int/enif_get_int round trip",
            &[74],
            "1",
        ),
        case(
            "nif_extra/int_get_ulong_ulong_max_roundtrip",
            "ULONG_MAX survives an enif_make_ulong/enif_get_ulong round trip",
            &[75],
            "1",
        ),
        case(
            "nif_extra/int_get_int_rejects_below_int_min",
            "enif_get_int refuses INT_MIN - 1",
            &[76],
            "1",
        ),
        // --- map build / lookup edges --------------------------------------------------------
        case(
            "nif_extra/map_make_map_from_arrays_two_pairs",
            "enif_make_map_from_arrays builds a 2-entry map with both values reachable",
            &[77],
            "1",
        ),
        case(
            "nif_extra/map_make_map_put_overwrites_existing_key",
            "enif_make_map_put on an existing key keeps the size and replaces the value",
            &[78],
            "1",
        ),
        // --- string / atom buffer edges ------------------------------------------------------
        case(
            "nif_extra/string_get_exactly_fitting_buffer",
            "enif_get_string fills an exactly-fitting LATIN1 buffer and answers 4",
            &[79],
            "4",
        ),
        case(
            "nif_extra/string_get_rejects_binary_end",
            "enif_get_string refuses a binary subject",
            &[80],
            "1",
        ),
        case(
            "nif_extra/atom_new_atom_utf8_name_reads_back",
            "a UTF-8 atom name reads back under ERL_NIF_UTF8 (4 bytes) and is refused under LATIN1",
            &[81],
            "1",
        ),
        case(
            "nif_extra/atom_get_atom_length_utf8_bytes",
            "enif_get_atom_length counts the UTF-8 byte budget of a multibyte atom (3)",
            &[82],
            "1",
        ),
        // --- time unit pairs ------------------------------------------------------------------
        case(
            "nif_extra/time_convert_nsec_to_usec",
            "enif_convert_time_unit maps 1e9 ns to 1e6 us",
            &[83],
            "1",
        ),
        // --- atom / string constructors that take an explicit length -------------------------
        case(
            "nif_extra/atom_make_atom_len_matches_make_atom",
            "enif_make_atom_len interns the same atom as the NUL-terminated enif_make_atom",
            &[84],
            "1",
        ),
        case(
            "nif_extra/atom_make_existing_atom_len_refuses_absent_name",
            "enif_make_existing_atom_len refuses a name the atom table does not hold",
            &[85],
            "1",
        ),
        case(
            "nif_extra/string_make_string_len_writes_prefix_only",
            "enif_make_string_len builds a list from exactly its length argument (\"hello\" len 3)",
            &[86],
            "1",
        ),
        // --- enif_make_copy across environments ----------------------------------------------
        case(
            "nif_extra/make_copy_tuple_into_fresh_env",
            "enif_make_copy moves a 2-tuple into a fresh enif_alloc_env environment",
            &[87],
            "1",
        ),
        // --- local pid / port accessors -------------------------------------------------------
        case(
            "nif_extra/process_get_local_pid_of_self",
            "enif_get_local_pid round trips the pid written by enif_make_pid(enif_self)",
            &[88],
            "1",
        ),
        case(
            "nif_extra/process_get_local_pid_rejects_non_pid",
            "enif_get_local_pid refuses a non-pid term",
            &[89],
            "1",
        ),
        case(
            "nif_extra/process_get_local_port_rejects_non_port",
            "enif_get_local_port refuses a non-port term",
            &[90],
            "1",
        ),
        // --- the negative predicate lattice ---------------------------------------------------
        case(
            "nif_extra/pred_is_ref_fun_port_false_for_tuple",
            "enif_is_ref, enif_is_fun and enif_is_port all refuse a 2-tuple",
            &[91],
            "1",
        ),
        case(
            "nif_extra/pred_is_exception_false_for_integer",
            "enif_is_exception is 0 for an ordinary term with no raise pending",
            &[92],
            "1",
        ),
        // --- enif_make_map_update (update-only, never insert) ---------------------------------
        case(
            "nif_extra/map_make_map_update_refuses_absent_key",
            "enif_make_map_update refuses a key the map does not hold",
            &[93],
            "1",
        ),
        case(
            "nif_extra/map_make_map_update_replaces_existing_value",
            "enif_make_map_update replaces an existing value and keeps the size at 1",
            &[94],
            "1",
        ),
        // --- the thread handle ----------------------------------------------------------------
        case(
            "nif_extra/thread_self_tid_equals_itself",
            "enif_equal_tids holds for the tid enif_thread_self reports twice",
            &[95],
            "1",
        ),        // --- enif_inspect_iolist_as_binary: the C's positional shape rules ------------------
        case(
            "nif_extra/iolist_refuses_bare_byte_integer",
            "enif_inspect_iolist_as_binary refuses a bare byte integer (a byte is only ever a cons head)",
            &[101],
            "1",
        ),
        case(
            "nif_extra/iolist_refuses_improper_tail",
            "enif_inspect_iolist_as_binary refuses the improper list [1|2]",
            &[102],
            "1",
        ),
        case(
            "nif_extra/iolist_refuses_non_byte_head",
            "enif_inspect_iolist_as_binary refuses the non-byte integer head [256]",
            &[103],
            "1",
        ),
        case(
            "nif_extra/iolist_refuses_bit_sized_element",
            "enif_inspect_iolist_as_binary refuses the non-byte-aligned element [<<1:7>>]",
            &[104],
            "1",
        ),
        case(
            "nif_extra/iolist_accepts_empty_list_size_zero",
            "enif_inspect_iolist_as_binary accepts [] with size 0",
            &[105],
            "1",
        ),
        case(
            "nif_extra/iolist_flattens_nested_list_and_binary",
            "enif_inspect_iolist_as_binary flattens [1,[2,<<3>>]] to <<1,2,3>>",
            &[106],
            "1",
        ),
        case(
            "nif_extra/iolist_accepts_byte_aligned_bitstring_element",
            "enif_inspect_iolist_as_binary accepts [<<1:8>>] and flattens it to <<1>>",
            &[107],
            "1",
        ),
        case(
            "nif_extra/iolist_accepts_byte_less_nested_list",
            "enif_inspect_iolist_as_binary accepts [[]] with size 0 and the non-NULL dummy data",
            &[108],
            "1",
        ),
        case(
            "nif_extra/iolist_accepts_binary_tail",
            "enif_inspect_iolist_as_binary accepts the binary tail [1|<<2>>] and flattens it",
            &[109],
            "1",
        ),        // --- enif_inspect_binary over a bit-offset window -----------------------------------
        case(
            "nif_extra/binary_inspect_bit_offset_window",
            "enif_inspect_binary accepts a byte-sized bit-offset window: size 40 + byte checksum",
            &[110],
            "40100960",
        ),
        case(
            "nif_extra/binary_inspect_refuses_non_byte_sized",
            "enif_inspect_binary refuses the non-byte-sized bitstring <<1:7>>",
            &[111],
            "1",
        ),
        // --- enif_make_sub_binary: bit-offset windows and the zero-size request -------------
        case(
            "nif_extra/sub_binary_bit_offset_window_from_zero",
            "enif_make_sub_binary windows a bit-offset source by bits (byte pos 0, 8 bytes)",
            &[120],
            "8005384",
        ),
        case(
            "nif_extra/sub_binary_bit_offset_window_from_four",
            "enif_make_sub_binary windows a bit-offset source by bits (byte pos 4, 8 bytes)",
            &[121],
            "8003358",
        ),
        case(
            "nif_extra/sub_binary_zero_size_is_empty",
            "enif_make_sub_binary with size 0 is the empty binary",
            &[122],
            "1",
        ),
        case(
            "nif_extra/sub_binary_zero_size_past_the_end_is_empty",
            "enif_make_sub_binary with size 0 accepts a pos one past the last byte and is empty",
            &[123],
            "1",
        ),
        // --- enif_get_string under ERL_NIF_UTF8: the truncation rule ------------------------
        case(
            "nif_extra/string_utf8_ascii_prefix_plus_whole_char_fits",
            "enif_get_string UTF-8 writes the ASCII prefix and one whole multibyte char (return 5)",
            &[130],
            "5",
        ),
        case(
            "nif_extra/string_utf8_drops_char_whole_one_byte_short",
            "enif_get_string UTF-8 drops the multibyte char whole when it does not fit (return -2)",
            &[131],
            "-2",
        ),
        case(
            "nif_extra/string_utf8_lone_multibyte_char_too_large",
            "enif_get_string UTF-8 answers -1 when only the multibyte char is present and cannot fit",
            &[132],
            "-1",
        ),
        // --- the widened binary / iolist shape block -----------------------------------------
        case(
            "nif_extra/binary_inspect_refuses_non_byte_sized_bitstring_again",
            "enif_inspect_binary still refuses a non-byte-aligned bitstring (<<1:7>>)",
            &[133],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_accepts_the_empty_binary",
            "enif_inspect_binary accepts <<>> with size 0 and enif_is_binary true",
            &[134],
            "1",
        ),
        case(
            "nif_extra/compare_three_byte_binaries_by_byte_value",
            "enif_compare orders <<1,2,3>> before <<1,2,4>>",
            &[135],
            "1",
        ),
        case(
            "nif_extra/identity_and_compare_of_a_binary_with_itself",
            "a binary is enif_is_identical to itself and enif_compare ranks it equal",
            &[136],
            "1",
        ),
        case(
            "nif_extra/iolist_nested_numeric_lists",
            "enif_inspect_iolist_as_binary flattens [[1,2],[3,4]] to four bytes",
            &[137],
            "1",
        ),
        case(
            "nif_extra/iolist_binary_element_then_byte",
            "enif_inspect_iolist_as_binary flattens [<<\"ab\">>,99] to three bytes",
            &[138],
            "1",
        ),
        case(
            "nif_extra/iolist_zero_byte_element",
            "enif_inspect_iolist_as_binary accepts the [0] element",
            &[139],
            "1",
        ),
        case(
            "nif_extra/iolist_highest_byte_element",
            "enif_inspect_iolist_as_binary accepts [255] as a byte element",
            &[140],
            "1",
        ),
        case(
            "nif_extra/iolist_element_is_an_empty_binary",
            "enif_inspect_iolist_as_binary accepts [<<>>] as the empty byte string",
            &[141],
            "1",
        ),
        case(
            "nif_extra/iolist_two_empty_nested_lists",
            "enif_inspect_iolist_as_binary accepts [[],[]] as the empty byte string",
            &[142],
            "1",
        ),
        case(
            "nif_extra/binary_sub_binary_window_of_four",
            "enif_make_sub_binary windows <<\"abcd\">> at byte 1 for two bytes",
            &[143],
            "2000296",
        ),
    ]
}
#[cfg(test)]
mod tests {
    //! Invariants of the widened A.3 group: every case is a module-run case claiming
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
            "the widened A.3 group must carry a broad selector set (got {})",
            cases.len()
        );
        for case in cases {
            assert!(
                case.id.starts_with("nif_extra/"),
                "case {} is outside the nif_extra group",
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
            assert_eq!(run.module, "nif_extra", "case {}", case.id);
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
                matches!(run.nif, Some(NifFixture::CompiledLibrary { name: "nif_extra", .. })),
                "case {}: the library must be the shared C-compiled nif_extra fixture",
                case.id
            );
            assert!(
                case.golden
                    .stdout
                    .expect("every case pins a golden")
                    .trim_start_matches('-')
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
    fn the_c_library_covers_the_new_accessor_and_constructor_families() {
        // Each family this group exists to add must really be referenced, or the case does not
        // exercise it.
        for symbol in [
            "enif_get_int64",
            "enif_get_uint64",
            "enif_make_uint64",
            "enif_make_int64",
            "enif_get_long",
            "enif_get_ulong",
            "enif_get_list_length",
            "enif_get_list_cell",
            "enif_is_empty_list",
            "enif_is_list",
            "enif_make_reverse_list",
            "enif_make_list_from_array",
            "enif_make_list_cell",
            "enif_get_tuple",
            "enif_is_tuple",
            "enif_make_tuple_from_array",
            "enif_inspect_binary",
            "enif_inspect_iolist_as_binary",
            "enif_make_sub_binary",
            "enif_realloc_binary",
            "enif_alloc_binary",
            "enif_make_binary",
            "enif_get_map_size",
            "enif_get_map_value",
            "enif_make_map_put",
            "enif_make_map_remove",
            "enif_make_map_from_arrays",
            "enif_map_iterator_is_head",
            "enif_map_iterator_is_tail",
            "enif_term_type",
            "enif_compare",
            "enif_is_identical",
            "enif_is_number",
            "enif_is_atom",
            "enif_get_string",
            "enif_get_string_length",
            "enif_make_string",
            "enif_get_atom",
            "enif_get_atom_length",
            "enif_term_size",
            "enif_hash",
            "enif_raise_exception",
            "enif_has_pending_exception",
            "enif_convert_time_unit",
            "enif_monotonic_time",
            "enif_make_uint",
            "enif_make_long",
            "enif_make_ulong",
            "enif_is_pid",
            "enif_is_map",
            "enif_make_pid",
            "enif_self",
            "enif_make_atom_len",
            "enif_make_existing_atom_len",
            "enif_make_string_len",
            "enif_make_copy",
            "enif_alloc_env",
            "enif_free_env",
            "enif_get_local_pid",
            "enif_get_local_port",
            "enif_is_ref",
            "enif_is_fun",
            "enif_is_port",
            "enif_is_exception",
            "enif_is_binary",
            "enif_make_map_update",
            "enif_thread_self",
            "enif_equal_tids",
        ] {
            assert!(
                super::NIF_EXTRA_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
    }

    #[test]
    fn the_c_library_avoids_the_divergent_and_variadic_slots() {
        // A call site for one of these would be a plain differing value (a mismatch), so the
        // library must never carry one:
        //   * enif_make_ref          -> the Rust runtime raises badarg (no host `make_ref`);
        //   * enif_term_to_binary    -> the Rust runtime answers 0 (no host `term_to_binary`);
        //   * the atom-cache slots   -> implementation-defined indices (always 0 here);
        //   * a *valid* enif_binary_to_term / a committed-atom lookup is exercised nowhere.
        for absent in [
            "enif_make_ref(",
            "enif_term_to_binary(",
            "enif_binary_to_term(",
            "enif_max_atom_cache_index(",
            "enif_get_atom_cache_index(",
            "ERL_NIF_INTERNAL_HASH",
        ] {
            assert!(
                !super::NIF_EXTRA_C.contains(absent),
                "the library must not reference {absent} (a guaranteed mismatch)"
            );
        }
        // A C-variadic slot must never be referenced: both runtimes dlopen the same `.so`, so an
        // unresolved slot would fail the Rust load where the oracle succeeds.
        for banned in ["enif_make_tuple(", "enif_make_list(", "enif_fprintf", "enif_set_option"] {
            assert!(
                !super::NIF_EXTRA_C.contains(banned),
                "the library must not reference the variadic {banned}"
            );
        }
    }

    #[test]
    fn the_module_exports_and_calls_the_probe() {
        for needle in [
            "-export([classify/1, probe/1, probe_term/2])",
            "nif_extra:probe(N)",
            "nif_extra:probe_term(N, built_term(N))",
            "erlang:load_nif(Dir ++ \"/nif_extra\", 0)",
        ] {
            assert!(
                super::NIF_EXTRA_MODULE_SOURCE.contains(needle),
                "the module must contain {needle:?}"
            );
        }
    }

    #[test]
    fn every_term_driven_selector_has_a_built_term() {
        // A selector routed to `probe_term/2` must have a `built_term/1` clause, or the module
        // raises `function_clause` and the case observes an error instead of the C's answer.
        let mut term_driven = 0;
        for case in group() {
            let CaseKind::Module(run) = &case.kind else {
                unreachable!("every case in this group is a module-run case")
            };
            if run.args[0] < 100 {
                continue;
            }
            term_driven += 1;
            assert!(
                super::NIF_EXTRA_MODULE_SOURCE.contains(&format!("built_term({}) ->", run.args[0])),
                "case {}: selector {} has no built_term/1 clause",
                case.id,
                run.args[0]
            );
        }
        assert!(term_driven >= 10, "the term-driven block must carry a broad selector set");
        // The two entry points really are both registered in the C table.
        for needle in ["{\"probe\", 1, probe, 0}", "{\"probe_term\", 2, probe_term, 0}"] {
            assert!(super::NIF_EXTRA_C.contains(needle), "the library must register {needle}");
        }
        for needle in ["probe(_) -> erlang:nif_error(not_loaded)", "probe_term(_, _) -> erlang:nif_error(not_loaded)"] {
            assert!(
                super::NIF_EXTRA_MODULE_SOURCE.contains(needle),
                "the module must stub every ErlNifFunc entry: {needle}"
            );
        }
    }

    #[test]
    fn the_widened_selector_blocks_have_a_c_arm_each() {
        // Every selector a case claims must have a real `case <sel>:` label in the shared library:
        // a missing arm would fall through to the `-1` "unknown selector" tail and the case would
        // pin that sentinel instead of the behaviour it names.
        for sel in (84..=95).chain(133..=143) {
            assert!(
                super::NIF_EXTRA_C.contains(&format!("case {sel}:")),
                "the library must carry a case arm for the widened selector {sel}"
            );
        }
    }
}