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

//! **Text / encoding / collection-list stdlib coverage**: the widened typed-IR
//! interpreter runs whole additional **system stdlib** modules over the shared
//! heap/atom host in `support`.
//!
//! Where `tests/stdlib_lists.rs` covers the `lists`/`ordsets`/`proplists` core and
//! `tests/stdlib_more.rs` covers `orddict`/`queue`/`gb_trees`/`gb_sets`, this file
//! adds the **string / byte-encoding / collection-list** surface:
//!
//! * **`string`** — the real OTP `string` module over `unicode_util` (case mapping
//!   `lowercase/1`/`titlecase/1`/`casefold/1`; `trim/2,3`; `slice/2,3`;
//!   `is_empty/1`; and the list-only split/token family `join/2`/`tokens/2`).
//!   The `string` exports that funnel through the unserved
//!   `unicode:characters_to_list/1` (`to_integer/1`, `split/2,3`, `find/2`) or
//!   `binary:compile_pattern/1` (`lexemes/2`) NIFs are recorded as **blocked
//!   mechanisms**, not faked);
//! * **`base64`** — the bit-syntax encoder/decoder (`decode/1`, and `encode/2`
//!   with its `#{padding => false}` / `#{mode => urlsafe}` option maps);
//! * **`unicode`** — the two normalising entry points
//!   (`characters_to_nfc_binary/1`, `characters_to_nfd_binary/1`) are recorded as
//!   **blocked** (they build their result through the `unicode` binary NIF);
//! * **`io_lib`** — the term/atom/string writers (`write/1` on an integer,
//!   `write_string/1`, `print/1`, `format/2`) and the character-list predicates
//!   (`char_list/1`, `deep_char_list/1`);
//! * **`lists`** — the reshaping/collection exports not covered elsewhere
//!   (`sort/1`, `append/2`, `concat/1`, `join/2`, `split/2`, `nthtail/2`,
//!   `keysort/2`, `enumerate/1`, `unzip/1`) plus the **fun-taking** exports
//!   (`any/2`, `all/2`, `foldr/3`, `mapfoldl/3`, `search/2`, `zf/2`) reached
//!   through the `stdlib_text_harness` fixture;
//! * **`ordsets`** — `new/0`, `to_list/1`, `is_set/1`, `is_empty/1` and the two
//!   **fun-taking** exports (`filter/2`, `fold/3`) through the harness.
//!
//! Every module here is loaded straight from the running Erlang's stdlib
//! (`<stdlib>/ebin/<module>.beam`, OTP 29 / `erts-17.1`, the differential oracle)
//! and its exports are run on a fresh [`beam_engine::Machine`] over the shared
//! heap/atom host. The harness fixture exists only because `lists:any/2`,
//! `lists:all/2`, `lists:foldr/3`, `lists:mapfoldl/3`, `lists:search/2`,
//! `lists:zf/2`, `ordsets:filter/2` and `ordsets:fold/3` cannot be entered
//! directly: the caller has to build an anonymous fun, so the wrapper drives
//! `make_fun3` + `call_fun*` + cross-module fun resolution into real `lists` /
//! `ordsets` (the `ExecutionContext::apply_fun` seam).
//!
//! Every expected value is the **real** system ERTS result, cross-checked with
//! `erl -noshell -eval …` (the exact command is quoted in an `oracle:` note next
//! to every assertion). The stdlib is located at **test time**; when
//! `erl`/stdlib is unavailable every test **skips loudly**. A case whose real
//! module still needs an unimplemented host BIF is **not** faked — it is recorded
//! as a blocked mechanism with its exact typed error
//! ([`blocked_mechanisms_report_their_typed_error`]).

mod support;

use beam_common::BeamError;
use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, assert_int_list, assert_int_pair_list, assert_small, assert_str, assert_term,
    chars, int_list, skip_reason, stdlib_ebin, term_atom, term_list, term_tuple, ModuleHost,
};

/// The committed text harness fixture (a real `.beam` from the system `erlc`).
const HARNESS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stdlib_text_harness.beam"
);

/// A host over the system stdlib `ebin` with the text harness registered.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    host.register("stdlib_text_harness", HARNESS_BEAM);
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
    term_atom(host, name);
    assert_atom(host, got, name);
}

/// Asserts `got` is a proper list of the Erlang strings `want`.
fn assert_str_list(host: &ModuleHost, got: Eterm, want: &[&str]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&element, want)) in elements.iter().zip(want.iter()).enumerate() {
        let codes: Vec<i64> = want.chars().map(|c| i64::from(c as u32)).collect();
        let got_codes = terms::list_elements(element)
            .unwrap_or_else(|_| panic!("element {index} is not a string: {element:?}"));
        assert_eq!(
            got_codes.len(),
            codes.len(),
            "element {index} length: got {got_codes:?}, want {codes:?}"
        );
        assert_str(host, element, want);
    }
}

/// Asserts `got` is the 2-tuple `{First, Second}` of two proper integer lists.
fn assert_pair_of_int_lists(host: &ModuleHost, got: Eterm, first: &[i64], second: &[i64]) {
    assert_eq!(
        terms::tuple_arity(got).ok(),
        Some(2),
        "a 2-tuple of lists: {got:?}"
    );
    assert_int_list(host, terms::tuple_element(got, 0).expect("first"), first);
    assert_int_list(host, terms::tuple_element(got, 1).expect("second"), second);
}

/// Builds an on-heap Erlang binary from raw bytes (a test-side `bs_create_bin`).
fn make_bin(host: &mut ModuleHost, bytes: &[u8]) -> Eterm {
    terms::make_heap_binary(host, bytes).expect("build an on-heap binary")
}

/// Asserts `got` is an Erlang binary whose bytes are `want`.
fn assert_bytes(got: Eterm, want: &[u8]) {
    let bytes = terms::binary_bytes(got).expect("a binary result");
    assert_eq!(bytes, want, "got {bytes:?}, want {want:?}");
}

// ---------------------------------------------------------------------------
// `string` — the real OTP string module (over `unicode_util`)
// ---------------------------------------------------------------------------

/// The `string` split/token family that runs without the `unicode` / `binary`
/// **NIFs**: `join/2` and `tokens/2`.
///
/// (`string:split/2,3`, `string:find/2`, `string:to_integer/1` and
/// `string:lexemes/2` all funnel through a NIF the test host does not serve —
/// see [`blocked_mechanisms_report_their_typed_error`].)
///
/// Families: lists (`put_list`/`get_list`), tuples, `++`/`--` (`erlang` BIFs),
/// `lists:reverse/2` + `lists:member/2` (the `lists` NIFs the host serves),
/// cross-module `string` calls.
#[test]
fn stdlib_string_join_and_tokens() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:join(["a","b","c"], "-")]),halt().'
    //         "a-b-c"
    let parts = {
        let a = chars(&mut host, "a");
        let b = chars(&mut host, "b");
        let c = chars(&mut host, "c");
        term_list(&mut host, &[a, b, c])
    };
    let sep = chars(&mut host, "-");
    let got = host
        .call("string", "join", &[parts, sep])
        .expect("string:join/2 must run");
    assert_str(&host, got, "a-b-c");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:tokens("a b c", " ")]),halt().'
    //         ["a","b","c"]
    let s = chars(&mut host, "a b c");
    let seps = chars(&mut host, " ");
    let got = host
        .call("string", "tokens", &[s, seps])
        .expect("string:tokens/2 must run");
    assert_str_list(&host, got, &["a", "b", "c"]);
}

/// `string:lowercase/1`, `string:titlecase/1` and `string:casefold/1` — the case
/// mappings, which walk `unicode_util`'s case tables.
///
/// Families: lists, tuples, cross-module `string` → `unicode_util` calls.
#[test]
fn stdlib_string_case_mappings() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:lowercase("ABC")]),halt().'
    //         "abc"
    let s = chars(&mut host, "ABC");
    let got = host
        .call("string", "lowercase", &[s])
        .expect("string:lowercase/1 must run");
    assert_str(&host, got, "abc");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:titlecase("hello world")]),halt().'
    //         "Hello world"
    let s = chars(&mut host, "hello world");
    let got = host
        .call("string", "titlecase", &[s])
        .expect("string:titlecase/1 must run");
    assert_str(&host, got, "Hello world");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:casefold("Hello")]),halt().'
    //         "hello"
    let s = chars(&mut host, "Hello");
    let got = host
        .call("string", "casefold", &[s])
        .expect("string:casefold/1 must run");
    assert_str(&host, got, "hello");
}

/// `string:is_empty/1`, `string:slice/2` and `string:slice/3`.
///
/// Families: lists, tuples, cross-module `string` calls.
#[test]
fn stdlib_string_is_empty_and_slice() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:is_empty("")]),halt().'
    //         true
    let empty = chars(&mut host, "");
    let got = host
        .call("string", "is_empty", &[empty])
        .expect("string:is_empty/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:is_empty("x")]),halt().'
    //         false
    let x = chars(&mut host, "x");
    let got = host
        .call("string", "is_empty", &[x])
        .expect("string:is_empty/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:slice("hello", 2)]),halt().'
    //         "llo"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "slice", &[s, make_small(2)])
        .expect("string:slice/2 must run");
    assert_str(&host, got, "llo");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:slice("hello", 1, 3)]),halt().'
    //         "ell"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "slice", &[s, make_small(1), make_small(3)])
        .expect("string:slice/3 must run");
    assert_str(&host, got, "ell");
}

/// `string:trim/2` and `string:trim/3` — the direction-aware trimmer over a set
/// of graphemes.
///
/// Families: lists, tuples, cross-module `string` → `unicode_util` calls.
#[test]
fn stdlib_string_trim_directions() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:trim("  hi  ", both)]),halt().'
    //         "hi"
    let s = chars(&mut host, "  hi  ");
    let both = term_atom(&mut host, "both");
    let got = host
        .call("string", "trim", &[s, both])
        .expect("string:trim/2 must run");
    assert_str(&host, got, "hi");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:trim("xxhixx", trailing, "x")]),halt().'
    //         "xxhi"
    let s = chars(&mut host, "xxhixx");
    let trailing = term_atom(&mut host, "trailing");
    let x = chars(&mut host, "x");
    let got = host
        .call("string", "trim", &[s, trailing, x])
        .expect("string:trim/3 must run");
    assert_str(&host, got, "xxhi");
}

// ---------------------------------------------------------------------------
// `base64` — the bit-syntax byte encoder/decoder
// ---------------------------------------------------------------------------

/// `base64:decode/1` — the decoder, built from `bs_add`/`bs_create_bin` bit
/// syntax.
///
/// Families: bitstrings (bitstring match/`bs_create_bin`/`bs_init_writable`),
/// lists, cross-module `base64` calls.
#[test]
fn stdlib_base64_decodes_padded_input() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[base64:decode(<<"aGVsbG8=">>)]),halt().'
    //         <<"hello">>
    let input = make_bin(&mut host, b"aGVsbG8=");
    let got = host
        .call("base64", "decode", &[input])
        .expect("base64:decode/1 must run");
    assert_bytes(got, b"hello");

    // oracle: erl -noshell -eval 'io:format("~p~n",[base64:decode(<<"aGk=">>)]),halt().'
    //         <<"hi">>
    let input = make_bin(&mut host, b"aGk=");
    let got = host
        .call("base64", "decode", &[input])
        .expect("base64:decode/1 must run");
    assert_bytes(got, b"hi");
}

/// `base64:encode/2` with an options map — the URL-safe alphabet and the
/// no-padding mode.
///
/// The option map is read with the pure-Erlang `maps:get/3` (a map-match
/// function, not the unserved `maps:get/2` BIF), so this runs end to end.
///
/// Families: maps (map literal construction + match), bitstrings
/// (`bs_create_bin`), lists, cross-module `base64` calls.
#[test]
fn stdlib_base64_encodes_with_options() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[base64:encode(<<"hi">>, #{padding => false})]),halt().'
    //         <<"aGk">>
    let input = make_bin(&mut host, b"hi");
    let options = {
        let padding = term_atom(&mut host, "padding");
        let false_atom = term_atom(&mut host, "false");
        terms::make_flatmap(&mut host, &[(padding, false_atom)]).expect("a flat options map")
    };
    let got = host
        .call("base64", "encode", &[input, options])
        .expect("base64:encode/2 must run");
    assert_bytes(got, b"aGk");

    // oracle: erl -noshell -eval 'io:format("~p~n",[base64:encode(<<251,239>>, #{mode => urlsafe})]),halt().'
    //         <<"--8=">>
    // (standard mode would emit `<<"++8=">>`; the URL-safe alphabet swaps `+`/`/`.)
    let input = make_bin(&mut host, &[251, 239]);
    let options = {
        let mode = term_atom(&mut host, "mode");
        let urlsafe = term_atom(&mut host, "urlsafe");
        terms::make_flatmap(&mut host, &[(mode, urlsafe)]).expect("a flat options map")
    };
    let got = host
        .call("base64", "encode", &[input, options])
        .expect("base64:encode/2 must run");
    assert_bytes(got, b"--8=");
}

// ---------------------------------------------------------------------------
// `io_lib` — the term/atom/string writers and the char-list predicates
// ---------------------------------------------------------------------------

/// `io_lib:write/1` (an integer), `io_lib:write_string/1` and `io_lib:print/1`.
///
/// These avoid `io_lib:write_atom/1` (which consults the reserved-word table —
/// see [`blocked_mechanisms_report_their_typed_error`]), so they run entirely
/// over `integer_to_list/1` + the bit/list writers.
///
/// Families: lists, tuples, cross-module `io_lib` → `io_lib_format` +
/// `io_lib_write` calls.
#[test]
fn stdlib_io_lib_writes_integers_and_strings() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:write(42)]),halt().'
    //         "42"
    let got = host
        .call("io_lib", "write", &[make_small(42)])
        .expect("io_lib:write/1 must run");
    assert_str(&host, got, "42");

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:write(-42)]),halt().'
    //         "-42"
    let got = host
        .call("io_lib", "write", &[make_small(-42)])
        .expect("io_lib:write/1 must run");
    assert_str(&host, got, "-42");

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:print(42)]),halt().'
    //         "42"
    let got = host
        .call("io_lib", "print", &[make_small(42)])
        .expect("io_lib:print/1 must run");
    assert_str(&host, got, "42");

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:write_string("hi")]),halt().'
    //         "\"hi\""
    let s = chars(&mut host, "hi");
    let got = host
        .call("io_lib", "write_string", &[s])
        .expect("io_lib:write_string/1 must run");
    assert_int_list(&host, got, &[34, 104, 105, 34]);
}

/// `io_lib:char_list/1` and `io_lib:deep_char_list/1`.
///
/// Families: lists, guards (`is_integer`/`is_list`), cross-module `io_lib` calls.
#[test]
fn stdlib_io_lib_char_list_predicates() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:char_list("abc")]),halt().'
    //         true
    let s = chars(&mut host, "abc");
    let got = host
        .call("io_lib", "char_list", &[s])
        .expect("io_lib:char_list/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:char_list([1,2,3])]),halt().'
    //         true
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("io_lib", "char_list", &[l])
        .expect("io_lib:char_list/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:deep_char_list(["abc",[1,2]])]),halt().'
    //         true
    let l = {
        let nested = int_list(&mut host, &[1, 2]);
        let inner = chars(&mut host, "abc");
        term_list(&mut host, &[inner, nested])
    };
    let got = host
        .call("io_lib", "deep_char_list", &[l])
        .expect("io_lib:deep_char_list/1 must run");
    assert_shared_atom(&mut host, got, "true");
}

/// `io_lib:format/2` for the `~w` integer control — a deep character list.
///
/// Families: lists, tuples, exceptions (`io_lib:format/2` wraps the formatter in
/// a `catch`), cross-module `io_lib` → `io_lib_format` calls.
#[test]
fn stdlib_io_lib_format_integer_controls() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:format("~w",[42])]),halt().'
    //         ["42"]
    let format = chars(&mut host, "~w");
    let args = term_list(&mut host, &[make_small(42)]);
    let got = host
        .call("io_lib", "format", &[format, args])
        .expect("io_lib:format/2 must run");
    let want = {
        let inner = chars(&mut host, "42");
        term_list(&mut host, &[inner])
    };
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:format("~w ~w",[1,2])]),halt().'
    //         ["1",32,"2"]
    let format = chars(&mut host, "~w ~w");
    let args = term_list(&mut host, &[make_small(1), make_small(2)]);
    let got = host
        .call("io_lib", "format", &[format, args])
        .expect("io_lib:format/2 must run");
    let want = {
        let one = chars(&mut host, "1");
        let two = chars(&mut host, "2");
        term_list(&mut host, &[one, make_small(32), two])
    };
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `lists` — the reshaping/collection exports not covered elsewhere
// ---------------------------------------------------------------------------

/// `lists:sort/1`, `lists:append/2` and `lists:concat/1` (over atoms).
///
/// Families: lists, `++` (`erlang` BIF), `atom_to_list`/`integer_to_list`
/// (`erlang` BIFs), cross-module `lists` calls — `concat/1` also drives a
/// **local** `lists` fun (`flatmap/2`) through `call_fun*`.
#[test]
fn stdlib_lists_sort_append_and_concat() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:sort([3,1,2])]),halt().'
    //         [1,2,3]
    let l = int_list(&mut host, &[3, 1, 2]);
    let got = host
        .call("lists", "sort", &[l])
        .expect("lists:sort/1 must run");
    assert_int_list(&host, got, &[1, 2, 3]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:append([1,2],[3,4])]),halt().'
    //         [1,2,3,4]
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[3, 4]);
    let got = host
        .call("lists", "append", &[a, b])
        .expect("lists:append/2 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:concat([a,b,c])]),halt().'
    //         "abc"
    let atoms = {
        let a = term_atom(&mut host, "a");
        let b = term_atom(&mut host, "b");
        let c = term_atom(&mut host, "c");
        term_list(&mut host, &[a, b, c])
    };
    let got = host
        .call("lists", "concat", &[atoms])
        .expect("lists:concat/1 must run");
    assert_str(&host, got, "abc");
}

/// `lists:join/2`, `lists:split/2`, `lists:nthtail/2` and `lists:unzip/1`.
///
/// Families: lists, tuples (`put_tuple2`), cross-module `lists` calls.
#[test]
fn stdlib_lists_join_split_nthtail_and_unzip() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:join(",",["a","b","c"])]),halt().'
    //         ["a",",","b",",","c"]
    let sep = chars(&mut host, ",");
    let parts = {
        let a = chars(&mut host, "a");
        let b = chars(&mut host, "b");
        let c = chars(&mut host, "c");
        term_list(&mut host, &[a, b, c])
    };
    let got = host
        .call("lists", "join", &[sep, parts])
        .expect("lists:join/2 must run");
    assert_str_list(&host, got, &["a", ",", "b", ",", "c"]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:split(2,[1,2,3])]),halt().'
    //         {[1,2],[3]}
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("lists", "split", &[make_small(2), l])
        .expect("lists:split/2 must run");
    assert_pair_of_int_lists(&host, got, &[1, 2], &[3]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:nthtail(2,[1,2,3])]),halt().'
    //         [3]
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("lists", "nthtail", &[make_small(2), l])
        .expect("lists:nthtail/2 must run");
    assert_int_list(&host, got, &[3]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:unzip([{1,10},{2,20}])]),halt().'
    //         {[1,2],[10,20]}
    let pairs = {
        let first = term_tuple(&mut host, &[make_small(1), make_small(10)]);
        let second = term_tuple(&mut host, &[make_small(2), make_small(20)]);
        term_list(&mut host, &[first, second])
    };
    let got = host
        .call("lists", "unzip", &[pairs])
        .expect("lists:unzip/1 must run");
    assert_pair_of_int_lists(&host, got, &[1, 2], &[10, 20]);
}

/// `lists:keysort/2` and `lists:enumerate/1`.
///
/// Families: lists, tuples, term ordering (`beam_term::cmp` driven by the
/// compiled `lists` code), cross-module `lists` calls.
#[test]
fn stdlib_lists_keysort_and_enumerate() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:keysort(1,[{2,20},{1,10}])]),halt().'
    //         [{1,10},{2,20}]
    let pairs = {
        let first = term_tuple(&mut host, &[make_small(2), make_small(20)]);
        let second = term_tuple(&mut host, &[make_small(1), make_small(10)]);
        term_list(&mut host, &[first, second])
    };
    let got = host
        .call("lists", "keysort", &[make_small(1), pairs])
        .expect("lists:keysort/2 must run");
    assert_int_pair_list(&host, got, &[(1, 10), (2, 20)]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:enumerate([10,20])]),halt().'
    //         [{1,10},{2,20}]
    let l = int_list(&mut host, &[10, 20]);
    let got = host
        .call("lists", "enumerate", &[l])
        .expect("lists:enumerate/1 must run");
    assert_int_pair_list(&host, got, &[(1, 10), (2, 20)]);
}

/// The fun-taking `lists` exports — `any/2`, `all/2`, `foldr/3`, `mapfoldl/3`,
/// `search/2` and `zf/2` — reached through the `stdlib_text_harness` wrappers.
///
/// Families: `make_fun3` + `call_fun*` (the cross-module `apply_fun` seam),
/// lists, tuples, booleans.
#[test]
fn stdlib_lists_fun_forms_via_harness() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -pa <fixtures> -eval 'io:format("~p~n",[stdlib_text_harness:lists_any_gt([1,2,3])]),halt().'
    //         true
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "lists_any_gt", &[l])
        .expect("harness lists_any_gt/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: ... stdlib_text_harness:lists_any_gt([0,1]) ... = false
    let l = int_list(&mut host, &[0, 1]);
    let got = host
        .call("stdlib_text_harness", "lists_any_gt", &[l])
        .expect("harness lists_any_gt/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: ... stdlib_text_harness:lists_all_pos([1,2,3]) ... = true
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "lists_all_pos", &[l])
        .expect("harness lists_all_pos/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: ... stdlib_text_harness:lists_all_pos([1,-1]) ... = false
    let l = int_list(&mut host, &[1, -1]);
    let got = host
        .call("stdlib_text_harness", "lists_all_pos", &[l])
        .expect("harness lists_all_pos/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: ... stdlib_text_harness:lists_foldr_sub([1,2,3]) ... = 2
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "lists_foldr_sub", &[l])
        .expect("harness lists_foldr_sub/1 must run");
    assert_small(&host, got, 2);

    // oracle: ... stdlib_text_harness:lists_mapfoldl_add([1,2,3]) ... = {[1,3,5],3}
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "lists_mapfoldl_add", &[l])
        .expect("harness lists_mapfoldl_add/1 must run");
    assert_eq!(terms::tuple_arity(got).ok(), Some(2), "a 2-tuple: {got:?}");
    assert_int_list(
        &host,
        terms::tuple_element(got, 0).expect("the mapped list"),
        &[1, 3, 5],
    );
    assert_small(&host, terms::tuple_element(got, 1).expect("the fold"), 3);

    // oracle: ... stdlib_text_harness:lists_search_gt([1,2,3,4]) ... = {value,3}
    let l = int_list(&mut host, &[1, 2, 3, 4]);
    let got = host
        .call("stdlib_text_harness", "lists_search_gt", &[l])
        .expect("harness lists_search_gt/1 must run");
    let want = {
        let value = term_atom(&mut host, "value");
        term_tuple(&mut host, &[value, make_small(3)])
    };
    assert_term(&host, got, want);

    // oracle: ... stdlib_text_harness:lists_search_gt([1,2]) ... = false
    let l = int_list(&mut host, &[1, 2]);
    let got = host
        .call("stdlib_text_harness", "lists_search_gt", &[l])
        .expect("harness lists_search_gt/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: ... stdlib_text_harness:lists_zf_double([1,2,3]) ... = [2,4,6]
    let l = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "lists_zf_double", &[l])
        .expect("harness lists_zf_double/1 must run");
    assert_int_list(&host, got, &[2, 4, 6]);
}

// ---------------------------------------------------------------------------
// `ordsets` — the ordered-set module
// ---------------------------------------------------------------------------

/// `ordsets:new/0`, `to_list/1`, `is_set/1` and `is_empty/1`.
///
/// Families: lists, cross-module `ordsets` calls.
#[test]
fn stdlib_ordsets_construction_and_predicates() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:new()]),halt().'
    //         []
    let empty = host
        .call("ordsets", "new", &[])
        .expect("ordsets:new/0 must run");
    assert!(terms::list_elements(empty).expect("a list").is_empty());

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:to_list([1,2,3])]),halt().'
    //         [1,2,3]
    let s = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("ordsets", "to_list", &[s])
        .expect("ordsets:to_list/1 must run");
    assert_int_list(&host, got, &[1, 2, 3]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:is_set([1,2,3])]),halt().'
    //         true
    let s = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("ordsets", "is_set", &[s])
        .expect("ordsets:is_set/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:is_set([2,1])]),halt().'
    //         false
    let s = int_list(&mut host, &[2, 1]);
    let got = host
        .call("ordsets", "is_set", &[s])
        .expect("ordsets:is_set/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:is_empty([])]),halt().'
    //         true
    let empty = int_list(&mut host, &[]);
    let got = host
        .call("ordsets", "is_empty", &[empty])
        .expect("ordsets:is_empty/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:is_empty([1])]),halt().'
    //         false
    let s = int_list(&mut host, &[1]);
    let got = host
        .call("ordsets", "is_empty", &[s])
        .expect("ordsets:is_empty/1 must run");
    assert_shared_atom(&mut host, got, "false");
}

/// The fun-taking `ordsets` exports — `filter/2` and `fold/3` — through the
/// harness.
///
/// Families: `make_fun3` + `call_fun*` (cross-module `apply_fun` into real
/// `ordsets`), lists.
#[test]
fn stdlib_ordsets_fun_forms_via_harness() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -pa <fixtures> -eval 'io:format("~p~n",[stdlib_text_harness:ordsets_filter_gt([1,2,3])]),halt().'
    //         [2,3]
    let s = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "ordsets_filter_gt", &[s])
        .expect("harness ordsets_filter_gt/1 must run");
    assert_int_list(&host, got, &[2, 3]);

    // oracle: ... stdlib_text_harness:ordsets_fold_rev([1,2,3]) ... = [3,2,1]
    let s = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_text_harness", "ordsets_fold_rev", &[s])
        .expect("harness ordsets_fold_rev/1 must run");
    assert_int_list(&host, got, &[3, 2, 1]);
}

// ---------------------------------------------------------------------------
// Blocked mechanisms (a host BIF gap, asserted exactly — never faked)
// ---------------------------------------------------------------------------

/// Probes the text/encoding candidates whose **real** module needs a host BIF the
/// test host does not serve. Each is printed as a `HOST GAP` line and asserted to
/// fail with the **exact typed error**, so a later regression that turns a typed
/// error into a *wrong result* (or a silent success) is caught.
///
/// These are the honest gaps of this node — the owning crate is `beam_bifs` (the
/// unserved BIF surface), not `beam_engine`.
#[test]
fn blocked_mechanisms_report_their_typed_error() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // ---- `io_lib:write/1` on a non-integer term --------------------------
    // Writing a term other than an integer/string consults the reserved-word
    // table (`erl_scan:reserved_word/1`), which is served by
    // `persistent_term:get/1,2` — a NIF the test host loads no library for.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:write(hello)]),halt().'
    //         "hello"
    let atom = term_atom(&mut host, "hello");
    let err = host
        .call("io_lib", "write", &[atom])
        .expect_err("`io_lib:write/1` on an atom needs the unserved `persistent_term:get/1`");
    println!("HOST GAP: io_lib:write/1 (an atom) — {err:?}");
    assert!(
        matches!(err, BeamError::Raise(_)),
        "unexpected `io_lib:write/1` (atom) error: {err:?}"
    );

    // ---- `io_lib:write_atom/1` ------------------------------------------
    // Same reserved-word lookup, entered directly.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:write_atom(hello)]),halt().'
    //         "hello"
    let atom = term_atom(&mut host, "hello");
    let err = host
        .call("io_lib", "write_atom", &[atom])
        .expect_err("`io_lib:write_atom/1` needs the unserved `persistent_term:get/1`");
    println!("HOST GAP: io_lib:write_atom/1 — {err:?}");
    assert!(
        matches!(err, BeamError::Raise(_)),
        "unexpected `io_lib:write_atom/1` error: {err:?}"
    );

    // ---- `unicode:characters_to_binary/3` --------------------------------
    // The three-argument form is a `unicode` **NIF** the host floor does not
    // serve (same family as the already-documented `characters_to_binary/1`).
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_binary([104,105], unicode, utf8)]),halt().'
    //         <<"hi">>
    let s = chars(&mut host, "hi");
    let input_encoding = term_atom(&mut host, "unicode");
    let output_encoding = term_atom(&mut host, "utf8");
    let err = host
        .call(
            "unicode",
            "characters_to_binary",
            &[s, input_encoding, output_encoding],
        )
        .expect_err("`unicode:characters_to_binary/3` is a NIF the host floor does not serve");
    println!("HOST GAP: unicode:characters_to_binary/3 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `unicode:characters_to_binary/3` error: {err:?}"
    );

    // ---- the `string` exports built on `unicode:characters_to_list/1` -----
    // `unicode:characters_to_list/1,2` is `erlang:nif_error(undef)` (a NIF), and
    // the host floor answers that stub with the typed `badarg` diagnostic rather
    // than a catchable raise — so a `string` export that funnels through it does
    // not reach its `catch` clause either. The three entries the objective names
    // are recorded here with their exact typed error instead of a fabricated
    // expected value.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[string:to_integer("123abc")]),halt().'
    //         {123,"abc"}
    let s = chars(&mut host, "123abc");
    let err = host
        .call("string", "to_integer", &[s])
        .expect_err("`string:to_integer/1` needs the `unicode:characters_to_list/1` NIF");
    println!("HOST GAP: string:to_integer/1 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `string:to_integer/1` error: {err:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:split("hello world", " ")]),halt().'
    //         ["hello","world"]
    let s = chars(&mut host, "hello world");
    let sep = chars(&mut host, " ");
    let err = host
        .call("string", "split", &[s, sep])
        .expect_err("`string:split/2` needs the `unicode:characters_to_list/1` NIF");
    println!("HOST GAP: string:split/2 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `string:split/2` error: {err:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:find("hello world", "world")]),halt().'
    //         "world"
    let s = chars(&mut host, "hello world");
    let pattern = chars(&mut host, "world");
    let err = host
        .call("string", "find", &[s, pattern])
        .expect_err("`string:find/2` needs the `unicode:characters_to_list/1` NIF");
    println!("HOST GAP: string:find/2 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `string:find/2` error: {err:?}"
    );

    // ---- `string:lexemes/2` ----------------------------------------------
    // A different missing facility: the separator scan reaches `search_compile/1`
    // → `binary:compile_pattern/1` and `unicode_util:gc/1` — the `binary:*` BIF
    // family the host floor does not serve.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[string:lexemes("a,b,,c", ",")]),halt().'
    //         ["a","b","c"]
    let s = chars(&mut host, "a,b,,c");
    let seps = chars(&mut host, ",");
    let err = host
        .call("string", "lexemes", &[s, seps])
        .expect_err("`string:lexemes/2` needs the unserved `binary:compile_pattern/1` BIF");
    println!("HOST GAP: string:lexemes/2 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `string:lexemes/2` error: {err:?}"
    );

    // ---- `unicode:characters_to_nfc_binary/1` / `..._nfd_binary/1` --------
    // These build their result through the `unicode:characters_to_binary/1` NIF
    // (`unicode.erl`'s `prepend_row_to_acc/2`), so only the empty input is
    // servable — see `tests/fixtures/stdlib_breadth_targets.txt`'s
    // `unicode:characters_to_nfc_list([])`.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_nfc_binary("abc")]),halt().'
    //         <<"abc">>
    let s = chars(&mut host, "abc");
    let err = host
        .call("unicode", "characters_to_nfc_binary", &[s])
        .expect_err("`unicode:characters_to_nfc_binary/1` needs the `unicode` binary NIF");
    println!("HOST GAP: unicode:characters_to_nfc_binary/1 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `unicode:characters_to_nfc_binary/1` error: {err:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_nfd_binary("abc")]),halt().'
    //         <<"abc">>
    let s = chars(&mut host, "abc");
    let err = host
        .call("unicode", "characters_to_nfd_binary", &[s])
        .expect_err("`unicode:characters_to_nfd_binary/1` needs the `unicode` binary NIF");
    println!("HOST GAP: unicode:characters_to_nfd_binary/1 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `unicode:characters_to_nfd_binary/1` error: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Robustness guard (does not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn the_located_stdlib_is_a_usable_stdlib() {
    // Mirrors `tests/stdlib_more.rs`: a genuinely independent check of the skip
    // guard's *input* — when the locator claims a stdlib, that directory must
    // actually hold the real `string.beam` this suite runs.
    let Some(dir) = stdlib_ebin() else {
        // No locatable stdlib at all: a genuine absent-optional-dependency skip.
        eprintln!("{}", skip_reason().unwrap_or_default());
        return;
    };
    assert!(
        dir.join("string.beam").is_file(),
        "the located oracle stdlib {dir:?} must hold the real `string.beam`"
    );
    assert!(
        dir.join("io_lib.beam").is_file(),
        "the located oracle stdlib {dir:?} must hold the real `io_lib.beam`"
    );
    assert!(
        skip_reason().is_none(),
        "a located, usable stdlib must not produce a skip reason"
    );
}
