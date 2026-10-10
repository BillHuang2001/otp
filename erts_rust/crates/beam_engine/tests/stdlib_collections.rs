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

//! A **collection / misc-module** slice of real system stdlib coverage.
//!
//! Where `tests/stdlib_more.rs` proves `orddict`/`queue`/`gb_trees`/`gb_sets`
//! and `tests/stdlib_sweep.rs` runs a curated spread over the installed OTP,
//! this file pins the *additional* collection/misc exports of **`calendar`**,
//! **`filename`**, **`array`**, **`dict`** and **`orddict`** — the pairs those
//! two suites do **not** already pin.
//!
//! Every module here is loaded straight from the running Erlang's stdlib
//! (`<stdlib>/ebin/<module>.beam`, OTP 29 / `erts-17.1`, the differential
//! oracle) and its exports are run on a fresh [`beam_engine::Machine`] over the
//! shared heap/atom host in `support`. Each expected value below is the **real**
//! system ERTS result, reproduced with a single batched
//! `erl -noshell -eval …` call (the exact command is quoted beside the
//! assertion) — never guessed.
//!
//! # What each group exercises
//!
//! * **`calendar`** — the tuple-form entry points plus the second-based
//!   conversions: integer arithmetic (`+`/`-`/`*`/`div`/`rem`), tuple
//!   construction/access, and the `{Year,Week}` ISO-week branch work.
//! * **`filename`** — string handling with cross-module calls into
//!   `lists`/`erlang`, an `os_type` guard (`system_info(os_type)`) and a
//!   `lists:append`-driven split/join.
//! * **`array`** — the tuple-backed structure: `erlang:make_tuple/2`-built
//!   interiors and `erlang:setelement/3` updates.
//! * **`dict`** — a hash dictionary whose non-trivial operations reach
//!   **captured-variable closures** (`call_fun`/`call_fun2`) and
//!   `erlang:phash/2` bucketing.
//! * **`orddict`** — an association list kept sorted by `lists`-recursion and
//!   fun-taking `lists` helpers.
//!
//! The stdlib is located at **test time**; when `erl`/stdlib is unavailable
//! every test **skips loudly**.

mod support;

use beam_engine::terms;
use beam_term::{is_atom, make_small, Eterm};
use support::{
    app_ebin, assert_atom, assert_int_list, assert_keyed_list, assert_small, assert_str,
    assert_term, chars, int_list, keyed_tuple, skip_reason, stdlib_ebin, term_list, term_tuple,
    ModuleHost,
};

/// A host over the system stdlib `ebin`, with `kernel`'s `ebin` on the search
/// path.
///
/// `filename`'s own `major_os_type/0` is `element(1, os:type())`, and `os` lives
/// in **kernel**, not stdlib — so the search path must reach kernel (exactly as
/// `tests/stdlib_sweep.rs` does) or every `filename` entry point that branches on
/// the OS type fails to resolve its callee.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    if let Some(kernel) = app_ebin("kernel") {
        host.add_search_dir(kernel);
    }
    host
}

/// Skips the calling test (loudly) when the oracle stdlib is unavailable;
/// returns `true` when the caller should return early.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// Asserts `got` is the atom `name`, interning the name first so the assertion
/// never depends on some *other* module having happened to mention it.
fn assert_shared_atom(host: &mut ModuleHost, got: Eterm, name: &str) {
    host.intern(name);
    assert_atom(host, got, name);
}

/// Asserts `got` is the proper list of the atoms `want`.
fn assert_atom_list(host: &ModuleHost, got: Eterm, want: &[&str]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &name)) in elements.iter().zip(want.iter()).enumerate() {
        assert!(is_atom(got), "element {index}: not an atom: {got:?}");
        assert_atom(host, got, name);
    }
}

/// A `{Year, Month, Day}` date tuple (the shape the tuple-form `calendar`
/// entry points take).
fn date(host: &mut ModuleHost, y: i64, m: i64, d: i64) -> Eterm {
    term_tuple(host, &[make_small(y), make_small(m), make_small(d)])
}

/// A `{Hour, Minute, Second}` time tuple.
fn time(host: &mut ModuleHost, h: i64, mi: i64, s: i64) -> Eterm {
    term_tuple(host, &[make_small(h), make_small(mi), make_small(s)])
}

/// A list of Erlang strings (one `chars/2` list per component).
fn str_list(host: &mut ModuleHost, parts: &[&str]) -> Eterm {
    let elements: Vec<Eterm> = parts.iter().map(|part| chars(host, part)).collect();
    term_list(host, &elements)
}

// ---------------------------------------------------------------------------
// `calendar` — tuple forms + second conversions
// ---------------------------------------------------------------------------

/// The tuple-form `calendar` conversions: `date_to_gregorian_days/1`,
/// `day_of_the_week/1` and `iso_week_number/1`.
///
/// Exercises integer arithmetic (`+`/`-`/`*`/`div`/`rem`), tuple
/// construction/access, and the `{Year,Week}` ISO-week branch work on a
/// genuinely different entry-point family than the `(Y,M,D)`-arity calls
/// `tests/stdlib_sweep.rs` pins.
#[test]
fn calendar_tuple_forms_compute_days_and_weeks() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:date_to_gregorian_days({2000,1,1})]),halt().'
    //         => 730485
    let day = date(&mut host, 2000, 1, 1);
    let days = host
        .call("calendar", "date_to_gregorian_days", &[day])
        .expect("calendar:date_to_gregorian_days/1 must run");
    assert_small(&host, days, 730485);

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:day_of_the_week({2000,1,1})]),halt().'
    //         => 6
    let weekday = host
        .call("calendar", "day_of_the_week", &[day])
        .expect("calendar:day_of_the_week/1 must run");
    assert_small(&host, weekday, 6);

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:iso_week_number({2000,1,1})]),halt().'
    //         => {1999,52}   (2000-01-01 is a Saturday, so it belongs to ISO week 1999-W52)
    let iso = host
        .call("calendar", "iso_week_number", &[day])
        .expect("calendar:iso_week_number/1 must run");
    let expected_iso = term_tuple(&mut host, &[make_small(1999), make_small(52)]);
    assert_term(&host, iso, expected_iso);

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:iso_week_number({2024,1,1})]),halt().'
    //         => {2024,1}    (2024-01-01 is a Monday)
    let new_year = date(&mut host, 2024, 1, 1);
    let iso = host
        .call("calendar", "iso_week_number", &[new_year])
        .expect("calendar:iso_week_number/1 must run");
    let expected_iso = term_tuple(&mut host, &[make_small(2024), make_small(1)]);
    assert_term(&host, iso, expected_iso);
}

/// The `calendar` second-based conversions that do run end to end:
/// `gregorian_seconds_to_datetime/1` and `seconds_to_time/1`.
#[test]
fn calendar_seconds_convert_to_datetime_and_time() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:gregorian_seconds_to_datetime(63113904000)]),halt().'
    //         => {{2000,1,1},{0,0,0}}
    let day = date(&mut host, 2000, 1, 1);
    let midnight = time(&mut host, 0, 0, 0);
    let seconds_term = make_small(63113904000);
    let back = host
        .call("calendar", "gregorian_seconds_to_datetime", &[seconds_term])
        .expect("calendar:gregorian_seconds_to_datetime/1 must run");
    let expected_datetime = term_tuple(&mut host, &[day, midnight]);
    assert_term(&host, back, expected_datetime);

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:seconds_to_time(3723)]),halt().'
    //         => {1,2,3}
    let secs = make_small(3723);
    let back = host
        .call("calendar", "seconds_to_time", &[secs])
        .expect("calendar:seconds_to_time/1 must run");
    let expected_time = time(&mut host, 1, 2, 3);
    assert_term(&host, back, expected_time);
}

/// The `calendar` exports that stop at the **test host's BIF floor** rather than
/// the engine: `time_to_seconds/1`'s compiled guard is `bif is_integer/3` (the
/// `is_integer(H, 0, 23)` **range** guard), and the host floor serves only the
/// `is_integer/1` form — so the whole call is the typed `badarg` the floor
/// answers a missing `erlang:` BIF with, never a wrong value.
///
/// `datetime_to_gregorian_seconds/1` is `86400 * date_to_gregorian_days(Date) +
/// time_to_seconds(Time)`, so it inherits the same gap.
///
/// Owner: the `beam_bifs`/host-floor `erlang:is_integer/3` BIF (the assertions
/// here pin the *typed* error so a later regression that turns it into a wrong
/// result is caught, mirroring
/// `tests/apps_lists.rs::blocked_targets_report_their_typed_error`).
#[test]
fn calendar_range_guard_targets_report_their_typed_error() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:time_to_seconds({1,2,3})]),halt().'
    //         => 3723
    let t = time(&mut host, 1, 2, 3);
    let err = host
        .call("calendar", "time_to_seconds", &[t])
        .expect_err("`calendar:time_to_seconds/1` needs the `erlang:is_integer/3` range guard");
    println!(
        "HOST GAP: calendar:time_to_seconds/1 — needs the host floor BIF \
         `erlang:is_integer/3`: {err:?}"
    );
    assert!(
        matches!(err, beam_common::BeamError::BadArg),
        "unexpected `calendar:time_to_seconds/1` typed error: {err:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~w~n",[calendar:datetime_to_gregorian_seconds({{2000,1,1},{0,0,0}})]),halt().'
    //         => 63113904000
    let day = date(&mut host, 2000, 1, 1);
    let midnight = time(&mut host, 0, 0, 0);
    let datetime = term_tuple(&mut host, &[day, midnight]);
    let err = host
        .call("calendar", "datetime_to_gregorian_seconds", &[datetime])
        .expect_err("`calendar:datetime_to_gregorian_seconds/1` calls `time_to_seconds/1`");
    println!(
        "HOST GAP: calendar:datetime_to_gregorian_seconds/1 — needs the host floor BIF \
         `erlang:is_integer/3`: {err:?}"
    );
    assert!(
        matches!(err, beam_common::BeamError::BadArg),
        "unexpected `calendar:datetime_to_gregorian_seconds/1` typed error: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// `filename` — join / split / nativename / flatten
// ---------------------------------------------------------------------------

/// `filename:join/2`, `split/1`, `nativename/1` and `flatten/1` — string
/// handling with a `system_info(os_type)` guard and cross-module `lists`
/// calls.
#[test]
fn filename_join_split_nativename_and_flatten() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:join("a","b")]),halt().'
    //         => "a/b"
    let a = chars(&mut host, "a");
    let b = chars(&mut host, "b");
    let joined = host
        .call("filename", "join", &[a, b])
        .expect("filename:join/2 must run");
    assert_str(&host, joined, "a/b");

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:join("/a","b")]),halt().'
    //         => "/a/b"
    let a = chars(&mut host, "/a");
    let b = chars(&mut host, "b");
    let joined = host
        .call("filename", "join", &[a, b])
        .expect("filename:join/2 must run");
    assert_str(&host, joined, "/a/b");

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:join([],"b")]),halt().'
    //         => "/b"    (an empty first component still writes the separator)
    let empty = chars(&mut host, "");
    let b = chars(&mut host, "b");
    let joined = host
        .call("filename", "join", &[empty, b])
        .expect("filename:join/2 must run");
    assert_str(&host, joined, "/b");

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:split("/a/b/c.txt")]),halt().'
    //         => ["/","a","b","c.txt"]
    let path = chars(&mut host, "/a/b/c.txt");
    let parts = host
        .call("filename", "split", &[path])
        .expect("filename:split/1 must run");
    let want = str_list(&mut host, &["/", "a", "b", "c.txt"]);
    assert_term(&host, parts, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:split("a/b/c")]),halt().'
    //         => ["a","b","c"]
    let path = chars(&mut host, "a/b/c");
    let parts = host
        .call("filename", "split", &[path])
        .expect("filename:split/1 must run");
    let want = str_list(&mut host, &["a", "b", "c"]);
    assert_term(&host, parts, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:nativename("/a//b")]),halt().'
    //         => "/a/b"    (the redundant separator is collapsed)
    let path = chars(&mut host, "/a//b");
    let native = host
        .call("filename", "nativename", &[path])
        .expect("filename:nativename/1 must run");
    assert_str(&host, native, "/a/b");

    // oracle: erl -noshell -eval 'io:format("~w~n",[filename:flatten("a/b/../c")]),halt().'
    //         => "a/b/../c"   (on unix `flatten/1` is the identity: it is the win32
    //                          branch that rewrites separators, so this pins the
    //                          unix `os_type` branch of the function)
    let path = chars(&mut host, "a/b/../c");
    let flat = host
        .call("filename", "flatten", &[path])
        .expect("filename:flatten/1 must run");
    assert_str(&host, flat, "a/b/../c");
}

// ---------------------------------------------------------------------------
// `array` — the tuple-backed structure
// ---------------------------------------------------------------------------

/// `array:new/0`, `set/3`, `reset/2` and `default/1` — the tuple-building
/// interiors (`erlang:make_tuple/2`) and `erlang:setelement/3` updates.
#[test]
fn array_new_set_reset_and_default() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[array:new() =:= array:from_list([])]),halt().'
    //         => true
    let fresh = host
        .call("array", "new", &[])
        .expect("array:new/0 must run");
    let from_empty = {
        let empty = int_list(&mut host, &[]);
        host.call("array", "from_list", &[empty])
            .expect("array:from_list/1 must run")
    };
    assert_term(&host, fresh, from_empty);

    // oracle: erl -noshell -eval 'io:format("~w~n",[{array:size(array:new()),array:to_list(array:new())}]),halt().'
    //         => {0,[]}
    let size = host
        .call("array", "size", &[fresh])
        .expect("array:size/1 must run");
    assert_small(&host, size, 0);
    let to_list = host
        .call("array", "to_list", &[fresh])
        .expect("array:to_list/1 must run");
    assert_int_list(&host, to_list, &[]);

    // oracle: erl -noshell -eval 'A = array:from_list([10,20,30]), B = array:set(1,99,A),
    //                              io:format("~w~n",[{array:get(1,B),array:to_list(B),array:size(B)}]),halt().'
    //         => {99,[10,99,30],3}
    let list = int_list(&mut host, &[10, 20, 30]);
    let array = host
        .call("array", "from_list", &[list])
        .expect("array:from_list/1 must run");
    let updated = host
        .call("array", "set", &[make_small(1), make_small(99), array])
        .expect("array:set/3 must run");
    let got = host
        .call("array", "get", &[make_small(1), updated])
        .expect("array:get/2 must run");
    assert_small(&host, got, 99);
    let got = host
        .call("array", "to_list", &[updated])
        .expect("array:to_list/1 must run");
    assert_int_list(&host, got, &[10, 99, 30]);
    let size = host
        .call("array", "size", &[updated])
        .expect("array:size/1 must run");
    assert_small(&host, size, 3);

    // oracle: erl -noshell -eval 'io:format("~w~n",[array:to_list(array:reset(1,array:from_list([10,20,30])))]),halt().'
    //         => [10,undefined,30]
    let reset = host
        .call("array", "reset", &[make_small(1), array])
        .expect("array:reset/2 must run");
    let got = host
        .call("array", "to_list", &[reset])
        .expect("array:to_list/1 must run");
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(elements.len(), 3, "a three-element array: {elements:?}");
    assert_small(&host, elements[0], 10);
    assert_atom(&host, elements[1], "undefined");
    assert_small(&host, elements[2], 30);

    // oracle: erl -noshell -eval 'io:format("~w~n",[array:default(array:from_list([10,20,30]))]),halt().'
    //         => undefined
    let default = host
        .call("array", "default", &[array])
        .expect("array:default/1 must run");
    assert_atom(&host, default, "undefined");
}

// ---------------------------------------------------------------------------
// `dict` — a hash dictionary (captured-variable closures + phash)
// ---------------------------------------------------------------------------

/// `dict:fetch_keys/1`, `erase/2`, `take/2`, `is_empty/1` and `append/3` over a
/// non-empty dictionary, i.e. the captured-variable-closure + `erlang:phash/2`
/// path (`store`/`fetch`/`find`/`size`/`is_key`/`to_list` are pinned by
/// `tests/stdlib_sweep.rs`).
#[test]
fn dict_fetch_keys_erase_take_and_append() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // Build the `[{a,1},{b,2}]` dict the assertions share.
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let pairs = term_list(&mut host, &[a, b]);
    let dict = host
        .call("dict", "from_list", &[pairs])
        .expect("dict:from_list/1 must run");

    // oracle: erl -noshell -eval 'io:format("~w~n",[dict:fetch_keys(dict:from_list([{a,1},{b,2}]))]),halt().'
    //         => [a,b]
    let keys = host
        .call("dict", "fetch_keys", &[dict])
        .expect("dict:fetch_keys/1 must run");
    assert_atom_list(&host, keys, &["a", "b"]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[{dict:is_empty(dict:new()),dict:is_empty(dict:from_list([{a,1}]))}]),halt().'
    //         => {true,false}
    let empty = host
        .call("dict", "new", &[])
        .expect("dict:new/0 must run");
    let got = host
        .call("dict", "is_empty", &[empty])
        .expect("dict:is_empty/1 must run");
    assert_shared_atom(&mut host, got, "true");
    let got = host
        .call("dict", "is_empty", &[dict])
        .expect("dict:is_empty/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: erl -noshell -eval 'D = dict:erase(a,dict:from_list([{a,1},{b,2}])),
    //                              io:format("~w~n",[{dict:fetch_keys(D),dict:size(D),dict:is_key(a,D)}]),halt().'
    //         => {[b],1,false}
    let a_atom = host.intern("a");
    let erased = host
        .call("dict", "erase", &[a_atom, dict])
        .expect("dict:erase/2 must run");
    let keys = host
        .call("dict", "fetch_keys", &[erased])
        .expect("dict:fetch_keys/1 must run");
    assert_atom_list(&host, keys, &["b"]);
    let size = host
        .call("dict", "size", &[erased])
        .expect("dict:size/1 must run");
    assert_small(&host, size, 1);
    let is_key = host
        .call("dict", "is_key", &[a_atom, erased])
        .expect("dict:is_key/2 must run");
    assert_shared_atom(&mut host, is_key, "false");

    // oracle: erl -noshell -eval 'D = dict:from_list([{a,1},{b,2}]), T = dict:take(a,D),
    //                              io:format("~w~n",[T]),halt().'
    //         => {1, <D minus a>}   -- element 2 is the erased dict
    let taken = host
        .call("dict", "take", &[a_atom, dict])
        .expect("dict:take/2 must run");
    assert_eq!(
        terms::tuple_arity(taken).ok(),
        Some(2),
        "dict:take/2 returns a 2-tuple: {taken:?}"
    );
    assert_small(&host, terms::tuple_element(taken, 0).expect("the value"), 1);
    let rest = terms::tuple_element(taken, 1).expect("the remaining dict");
    assert_term(&host, rest, erased);

    // oracle: erl -noshell -eval 'D = dict:append(a,9,dict:new()),
    //                              io:format("~w~n",[{dict:fetch_keys(D),dict:fetch(a,D)}]),halt().'
    //         => {[a],[9]}
    let appended = host
        .call("dict", "append", &[a_atom, make_small(9), empty])
        .expect("dict:append/3 must run");
    let keys = host
        .call("dict", "fetch_keys", &[appended])
        .expect("dict:fetch_keys/1 must run");
    assert_atom_list(&host, keys, &["a"]);
    let value = host
        .call("dict", "fetch", &[a_atom, appended])
        .expect("dict:fetch/2 must run");
    assert_int_list(&host, value, &[9]);

    // oracle: erl -noshell -eval 'D0 = dict:append(a,9,dict:new()), D = dict:append(a,10,D0),
    //                              io:format("~w~n",[dict:fetch(a,D)]),halt().'
    //         => [9,10]
    let appended = host
        .call("dict", "append", &[a_atom, make_small(10), appended])
        .expect("dict:append/3 must run");
    let value = host
        .call("dict", "fetch", &[a_atom, appended])
        .expect("dict:fetch/2 must run");
    assert_int_list(&host, value, &[9, 10]);
}

// ---------------------------------------------------------------------------
// `orddict` — an association list kept sorted
// ---------------------------------------------------------------------------

/// `orddict:from_list/1`, `fetch_keys/1`, `take/2` and `append/3` — the pairs
/// `tests/stdlib_more.rs` does not cover (`new`/`store`/`find`/`fetch`/`size`/
/// `is_key`/`is_empty`/`erase`/`map`/`filter`/`update`/`fold` are pinned there).
#[test]
fn orddict_from_list_fetch_keys_take_and_append() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:from_list([{b,2},{a,1}])]),halt().'
    //         => [{a,1},{b,2}]   (an unsorted input list is ordered by key)
    let b2 = keyed_tuple(&mut host, "b", 2);
    let a1 = keyed_tuple(&mut host, "a", 1);
    let unsorted = term_list(&mut host, &[b2, a1]);
    let dict = host
        .call("orddict", "from_list", &[unsorted])
        .expect("orddict:from_list/1 must run");
    assert_keyed_list(&host, dict, &[("a", 1), ("b", 2)]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:from_list([{a,1},{a,2}])]),halt().'
    //         => [{a,2}]   (a duplicate key collapses last-value-wins)
    let first = keyed_tuple(&mut host, "a", 1);
    let second = keyed_tuple(&mut host, "a", 2);
    let dup = term_list(&mut host, &[first, second]);
    let dict2 = host
        .call("orddict", "from_list", &[dup])
        .expect("orddict:from_list/1 must run");
    assert_keyed_list(&host, dict2, &[("a", 2)]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:fetch_keys([{a,1},{b,2}])]),halt().'
    //         => [a,b]
    let keys = host
        .call("orddict", "fetch_keys", &[dict])
        .expect("orddict:fetch_keys/1 must run");
    assert_atom_list(&host, keys, &["a", "b"]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:take(a,[{a,1},{b,2}])]),halt().'
    //         => {1,[{b,2}]}
    let a_atom = host.intern("a");
    let taken = host
        .call("orddict", "take", &[a_atom, dict])
        .expect("orddict:take/2 must run");
    assert_eq!(
        terms::tuple_arity(taken).ok(),
        Some(2),
        "orddict:take/2 returns a 2-tuple: {taken:?}"
    );
    assert_small(&host, terms::tuple_element(taken, 0).expect("the value"), 1);
    assert_keyed_list(
        &host,
        terms::tuple_element(taken, 1).expect("the remaining orddict"),
        &[("b", 2)],
    );

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:take(z,[{a,1},{b,2}])]),halt().'
    //         => error
    let z_atom = host.intern("z");
    let missing = host
        .call("orddict", "take", &[z_atom, dict])
        .expect("orddict:take/2 must run");
    assert_shared_atom(&mut host, missing, "error");

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:append(a,2,[{a,[1]}])]),halt().'
    //         => [{a,[1,2]}]   (appended onto the value list of an existing key)
    let a = host.intern("a");
    let one = int_list(&mut host, &[1]);
    let entry = term_tuple(&mut host, &[a, one]);
    let dict = term_list(&mut host, &[entry]);
    let appended = host
        .call("orddict", "append", &[a, make_small(2), dict])
        .expect("orddict:append/3 must run");
    let expected_value = int_list(&mut host, &[1, 2]);
    let expected_entry = term_tuple(&mut host, &[a, expected_value]);
    let expected = term_list(&mut host, &[expected_entry]);
    assert_term(&host, appended, expected);

    // oracle: erl -noshell -eval 'io:format("~w~n",[orddict:append(c,3,[{a,[1]}])]),halt().'
    //         => [{a,[1]},{c,[3]}]   (a new key gets a fresh one-element list)
    let c = host.intern("c");
    let a = host.intern("a");
    let one = int_list(&mut host, &[1]);
    let entry = term_tuple(&mut host, &[a, one]);
    let dict = term_list(&mut host, &[entry]);
    let appended = host
        .call("orddict", "append", &[c, make_small(3), dict])
        .expect("orddict:append/3 must run");
    let a_one = int_list(&mut host, &[1]);
    let a_entry = term_tuple(&mut host, &[a, a_one]);
    let c_three = int_list(&mut host, &[3]);
    let c_entry = term_tuple(&mut host, &[c, c_three]);
    let expected = term_list(&mut host, &[a_entry, c_entry]);
    assert_term(&host, appended, expected);
}
