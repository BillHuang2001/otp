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

//! The **second half** of the text / encoding / collection-list stdlib coverage,
//! split out of `tests/stdlib_text.rs` to keep both files near the ~1000-line
//! target.
//!
//! Where `stdlib_text.rs` covers the `string` case/slice/trim/token surface, the
//! `base64` bit-syntax codec, the `io_lib` writers, the reshaping `lists`
//! exports and `ordsets`, this file adds:
//!
//! * **`unicode`** — the *positive* character-property surface (this module has
//!   no direct export that runs without the `unicode` binary NIF in the parent
//!   file, so its reachable predicates are collected here):
//!   `is_whitespace/1`, `is_id_start/1`, `is_id_continue/1`, `category/1`,
//!   `bin_is_7bit/1`, the BOM helpers `bom_to_encoding/1` / `encoding_to_bom/1`,
//!   and the list-only normalisers `characters_to_nfkc_list/1` /
//!   `characters_to_nfkd_list/1`;
//! * **`string`** — the obsolete-API and alignment helpers that run without the
//!   `unicode` NIFs: `strip/1`, `left/2`, `right/2`, `substr/2,3`, `equal/2`,
//!   `prefix/2`, `centre/2,3`, `pad/2`;
//! * **`io_lib`** — `fwrite/2`, `nl/0`, `printable_list/1` and
//!   `printable_unicode_list/1`;
//! * **`lists`** — the tuple-key family (`keydelete/3`, `keymember/3`,
//!   `keyreplace/4`, `keytake/3`), `prefix/2`, `suffix/2` and the ordered
//!   `merge/1,2`;
//! * **`ordsets`** — the n-ary `union/1` and `intersection/1`.
//!
//! Every expected value is the **real** system ERTS result, cross-checked with
//! `erl -noshell -eval …` (the exact command is quoted in an `oracle:` note next
//! to every assertion). The stdlib is located at **test time**; when
//! `erl`/stdlib is unavailable every test **skips loudly**. No value is faked.

mod support;

use beam_common::BeamError;
use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, assert_int_list, assert_keyed_list, assert_str, assert_term, chars, int_list,
    keyed_tuple, skip_reason, stdlib_ebin, term_atom, term_list, term_tuple, ModuleHost,
};

/// A host over the system stdlib `ebin` (no fixture modules needed here).
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    ModuleHost::new(ebin)
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

/// Builds an on-heap Erlang binary from raw bytes (a test-side `bs_create_bin`).
fn make_bin(host: &mut ModuleHost, bytes: &[u8]) -> Eterm {
    terms::make_heap_binary(host, bytes).expect("build an on-heap binary")
}

/// Asserts `got` is an Erlang binary whose bytes are `want`.
fn assert_bytes(got: Eterm, want: &[u8]) {
    let bytes = terms::binary_bytes(got).expect("a binary result");
    assert_eq!(bytes, want, "got {bytes:?}, want {want:?}");
}

/// Asserts `got` is the 2-tuple `{First, Second}` of two atoms.
fn assert_atom_pair(host: &mut ModuleHost, got: Eterm, first: &str, second: &str) {
    assert_eq!(
        terms::tuple_arity(got).ok(),
        Some(2),
        "a 2-tuple: {got:?}"
    );
    let first = term_atom(host, first);
    let second = term_atom(host, second);
    let want = term_tuple(host, &[first, second]);
    assert_term(host, got, want);
}

/// Runs `module:function` and asserts it fails with **exactly** the typed
/// `BeamError::BadArg` the host's unserved NIF seam produces — a HOST GAP
/// (owner: `beam_bifs`), never pinned as a passing value.
fn assert_badarg_gap(host: &mut ModuleHost, module: &str, function: &str, args: &[Eterm]) {
    let err = host
        .call(module, function, args)
        .expect_err("a host-gap call must fail, never return a fabricated value");
    assert!(
        matches!(err, BeamError::BadArg),
        "{module}:{function} expected the typed BadArg host gap, got {err:?}"
    );
    eprintln!("HOST GAP: {module}:{function} — {err:?}");
}

// ---------------------------------------------------------------------------
// `unicode` — the character-property / BOM / normalisation-list surface
// ---------------------------------------------------------------------------

/// `unicode:is_whitespace/1`, `is_id_start/1`, `is_id_continue/1` and
/// `category/1` — the pure predicate/classification surface over
/// `unicode_util`'s generated tables.
///
/// Families: guards (`is_integer`), tuples, cross-module `unicode` →
/// `unicode_util` calls.
#[test]
fn stdlib_unicode_character_predicates() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:is_whitespace(32)]),halt().'
    //         true
    let got = host
        .call("unicode", "is_whitespace", &[make_small(32)])
        .expect("unicode:is_whitespace/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:is_whitespace(97)]),halt().'
    //         false
    let got = host
        .call("unicode", "is_whitespace", &[make_small(97)])
        .expect("unicode:is_whitespace/1 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:is_id_start(97)]),halt().'
    //         true
    let got = host
        .call("unicode", "is_id_start", &[make_small(97)])
        .expect("unicode:is_id_start/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:is_id_continue(48)]),halt().'
    //         true
    let got = host
        .call("unicode", "is_id_continue", &[make_small(48)])
        .expect("unicode:is_id_continue/1 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:category(97)]),halt().'
    //         {letter,lowercase}
    let got = host
        .call("unicode", "category", &[make_small(97)])
        .expect("unicode:category/1 must run");
    assert_atom_pair(&mut host, got, "letter", "lowercase");
}

/// `unicode:bom_to_encoding/1` and `unicode:encoding_to_bom/1` — the BOM
/// helpers, which are pure bit-syntax pattern matches / builders.
///
/// Families: bitstrings (`bs_start_match` + `bs_create_bin` / binary literals),
/// tuples, lists.
#[test]
fn stdlib_unicode_bom_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:bom_to_encoding(<<255,254>>)]),halt().'
    //         {{utf16,little},2}
    let bom = make_bin(&mut host, &[255, 254]);
    let got = host
        .call("unicode", "bom_to_encoding", &[bom])
        .expect("unicode:bom_to_encoding/1 must run");
    let want = {
        let utf16 = term_atom(&mut host, "utf16");
        let little = term_atom(&mut host, "little");
        let encoding = term_tuple(&mut host, &[utf16, little]);
        term_tuple(&mut host, &[encoding, make_small(2)])
    };
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:bom_to_encoding(<<>>)]),halt().'
    //         {latin1,0}
    let empty = make_bin(&mut host, &[]);
    let got = host
        .call("unicode", "bom_to_encoding", &[empty])
        .expect("unicode:bom_to_encoding/1 must run");
    let want = {
        let latin1 = term_atom(&mut host, "latin1");
        term_tuple(&mut host, &[latin1, make_small(0)])
    };
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:encoding_to_bom(utf16)]),halt().'
    //         <<254,255>>
    let utf16 = term_atom(&mut host, "utf16");
    let got = host
        .call("unicode", "encoding_to_bom", &[utf16])
        .expect("unicode:encoding_to_bom/1 must run");
    assert_bytes(got, &[254, 255]);
}

/// `unicode:characters_to_nfkc_list/1` and `characters_to_nfkd_list/1` — the
/// list-only normalisers (their `*_binary` twins need the `unicode` NIF; see
/// `tests/stdlib_text.rs`).
///
/// Families: lists, tuples, cross-module `unicode` → `unicode_util` calls.
#[test]
fn stdlib_unicode_nfkc_nfkd_lists() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_nfkc_list([104,105])]),halt().'
    //         "hi"
    let input = int_list(&mut host, &[104, 105]);
    let got = host
        .call("unicode", "characters_to_nfkc_list", &[input])
        .expect("unicode:characters_to_nfkc_list/1 must run");
    assert_str(&host, got, "hi");

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_nfkd_list([104,105])]),halt().'
    //         "hi"
    let input = int_list(&mut host, &[104, 105]);
    let got = host
        .call("unicode", "characters_to_nfkd_list", &[input])
        .expect("unicode:characters_to_nfkd_list/1 must run");
    assert_str(&host, got, "hi");
}

// ---------------------------------------------------------------------------
// `string` — the obsolete-API and alignment helpers
// ---------------------------------------------------------------------------

/// `string:strip/1`, `left/2`, `right/2`, `substr/2`, `substr/3`, `equal/2` and
/// `prefix/2`.
///
/// Families: lists (`put_list`/`get_list`), guards, `lists` NIFs, cross-module
/// `string` calls.
#[test]
fn stdlib_string_obsolete_and_slice_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:strip("  hi  ")]),halt().'
    //         "hi"
    let s = chars(&mut host, "  hi  ");
    let got = host
        .call("string", "strip", &[s])
        .expect("string:strip/1 must run");
    assert_str(&host, got, "hi");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:left("hello", 2)]),halt().'
    //         "he"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "left", &[s, make_small(2)])
        .expect("string:left/2 must run");
    assert_str(&host, got, "he");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:right("hello", 2)]),halt().'
    //         "lo"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "right", &[s, make_small(2)])
        .expect("string:right/2 must run");
    assert_str(&host, got, "lo");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:substr("hello", 2)]),halt().'
    //         "ello"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "substr", &[s, make_small(2)])
        .expect("string:substr/2 must run");
    assert_str(&host, got, "ello");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:substr("hello", 2, 3)]),halt().'
    //         "ell"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "substr", &[s, make_small(2), make_small(3)])
        .expect("string:substr/3 must run");
    assert_str(&host, got, "ell");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:equal("hi","hi")]),halt().'
    //         true
    let a = chars(&mut host, "hi");
    let b = chars(&mut host, "hi");
    let got = host
        .call("string", "equal", &[a, b])
        .expect("string:equal/2 must run");
    assert_shared_atom(&mut host, got, "true");

    // HOST GAP: `string:prefix/2` funnels through `unicode:characters_to_list/1`,
    // an unserved `unicode` NIF (owner: `beam_bifs`) — typed `BadArg`, not a value.
    // oracle: erl -noshell -eval 'io:format("~p~n",[string:prefix("hello","he")]),halt().'
    //         "llo"
    let s = chars(&mut host, "hello");
    let prefix = chars(&mut host, "he");
    assert_badarg_gap(&mut host, "string", "prefix", &[s, prefix]);
}

/// `string:centre/2` and `string:pad/2` — the alignment helpers (`pad/2` returns
/// an iolist, so its result is a list whose first element is a string).
///
/// Families: lists, `lists` NIFs, cross-module `string` calls.
#[test]
fn stdlib_string_alignment_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:centre("ab", 6)]),halt().'
    //         "  ab  "
    let s = chars(&mut host, "ab");
    let got = host
        .call("string", "centre", &[s, make_small(6)])
        .expect("string:centre/2 must run");
    assert_str(&host, got, "  ab  ");

    // oracle: erl -noshell -eval 'io:format("~p~n",[string:pad("5", 3)]),halt().'
    //         ["5",32,32]
    let s = chars(&mut host, "5");
    let got = host
        .call("string", "pad", &[s, make_small(3)])
        .expect("string:pad/2 must run");
    let want = {
        let five = chars(&mut host, "5");
        term_list(&mut host, &[five, make_small(32), make_small(32)])
    };
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `io_lib` — `fwrite/2`, `nl/0` and the printability predicates
// ---------------------------------------------------------------------------

/// `io_lib:fwrite/2`, `io_lib:nl/0`, `io_lib:printable_list/1` and
/// `io_lib:printable_unicode_list/1`.
///
/// Families: lists, tuples, exceptions (`fwrite/2` wraps the formatter),
/// cross-module `io_lib` → `io_lib_format` calls.
#[test]
fn stdlib_io_lib_fwrite_and_printability() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:fwrite("~w",[42])]),halt().'
    //         ["42"]
    let format = chars(&mut host, "~w");
    let args = term_list(&mut host, &[make_small(42)]);
    let got = host
        .call("io_lib", "fwrite", &[format, args])
        .expect("io_lib:fwrite/2 must run");
    let want = {
        let inner = chars(&mut host, "42");
        term_list(&mut host, &[inner])
    };
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:nl()]),halt().'
    //         "\n"
    let got = host.call("io_lib", "nl", &[]).expect("io_lib:nl/0 must run");
    assert_int_list(&host, got, &[10]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:printable_list("abc")]),halt().'
    //         true
    // HOST GAP: `io_lib:printable_list/1` consults `io:printable_range/0` (a
    // startup-flag facility the host does not serve; owner: `beam_bifs`) — BadArg.
    let s = chars(&mut host, "abc");
    assert_badarg_gap(&mut host, "io_lib", "printable_list", &[s]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[io_lib:printable_unicode_list("abc")]),halt().'
    //         true
    let s = chars(&mut host, "abc");
    let got = host
        .call("io_lib", "printable_unicode_list", &[s])
        .expect("io_lib:printable_unicode_list/1 must run");
    assert_shared_atom(&mut host, got, "true");
}

// ---------------------------------------------------------------------------
// `lists` — the tuple-key family, prefix/suffix and ordered merge
// ---------------------------------------------------------------------------

/// `lists:keydelete/3`, `keymember/3`, `keyreplace/4` and `keytake/3` — the
/// tuple-key operations over `{Key, Value}` pairs (all four are `lists` NIFs).
///
/// Families: tuples, lists, `call_ext` into the real `lists` NIFs.
#[test]
fn stdlib_lists_key_operations() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    /// The `[{a,1},{b,2},{c,3}]` list the four keyed operations share.
    fn abc(host: &mut ModuleHost) -> Eterm {
        let a = keyed_tuple(host, "a", 1);
        let b = keyed_tuple(host, "b", 2);
        let c = keyed_tuple(host, "c", 3);
        term_list(host, &[a, b, c])
    }

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:keydelete(b,1,[{a,1},{b,2},{c,3}])]),halt().'
    //         [{a,1},{c,3}]
    let list = abc(&mut host);
    let key = term_atom(&mut host, "b");
    let got = host
        .call("lists", "keydelete", &[key, make_small(1), list])
        .expect("lists:keydelete/3 must run");
    assert_eq!(terms::list_elements(got).expect("a list").len(), 2);
    assert_eq!(
        terms::tuple_element(terms::list_elements(got).expect("a list")[1], 0).expect("key"),
        term_atom(&mut host, "c")
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:keymember(b,1,[{a,1},{b,2}])]),halt().'
    //         true
    // HOST GAP: `lists:keymember/3` is a BIF placeholder (`erlang:nif_error(undef)`)
    // the host does not serve (owner: `beam_bifs`) — typed BadArg.
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let list = term_list(&mut host, &[a, b]);
    let key = term_atom(&mut host, "b");
    assert_badarg_gap(&mut host, "lists", "keymember", &[key, make_small(1), list]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:keyreplace(b,1,[{a,1},{b,2}],{b,9})]),halt().'
    //         [{a,1},{b,9}]
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let list = term_list(&mut host, &[a, b]);
    let key = term_atom(&mut host, "b");
    let replacement = term_tuple(&mut host, &[key, make_small(9)]);
    let got = host
        .call("lists", "keyreplace", &[key, make_small(1), list, replacement])
        .expect("lists:keyreplace/4 must run");
    // The keys are the atoms `a` / `b` (values are the small ints), so compare
    // structurally as `{atom, small integer}` pairs, not as an integer pair list.
    assert_keyed_list(&host, got, &[("a", 1), ("b", 9)]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:keytake(b,1,[{a,1},{b,2}])]),halt().'
    //         {value,{b,2},[{a,1}]}
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let list = term_list(&mut host, &[a, b]);
    let key = term_atom(&mut host, "b");
    let got = host
        .call("lists", "keytake", &[key, make_small(1), list])
        .expect("lists:keytake/3 must run");
    let value = term_atom(&mut host, "value");
    let taken = keyed_tuple(&mut host, "b", 2);
    let rest = {
        let a = keyed_tuple(&mut host, "a", 1);
        term_list(&mut host, &[a])
    };
    let want = term_tuple(&mut host, &[value, taken, rest]);
    assert_term(&host, got, want);
}

/// `lists:prefix/2`, `lists:suffix/2`, `lists:merge/2` and `lists:merge/1`.
///
/// Families: lists, `lists` NIFs / recursive `lists` code, `call_ext` into the
/// real module.
#[test]
fn stdlib_lists_prefix_suffix_and_merge() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:prefix([1,2],[1,2,3])]),halt().'
    //         true
    let prefix = int_list(&mut host, &[1, 2]);
    let list = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("lists", "prefix", &[prefix, list])
        .expect("lists:prefix/2 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:suffix([2,3],[1,2,3])]),halt().'
    //         true
    let suffix = int_list(&mut host, &[2, 3]);
    let list = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("lists", "suffix", &[suffix, list])
        .expect("lists:suffix/2 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:merge([1,3],[2,4])]),halt().'
    //         [1,2,3,4]
    let a = int_list(&mut host, &[1, 3]);
    let b = int_list(&mut host, &[2, 4]);
    let got = host
        .call("lists", "merge", &[a, b])
        .expect("lists:merge/2 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:merge([[1,3],[2,4]])]),halt().'
    //         [1,2,3,4]
    let lists = {
        let a = int_list(&mut host, &[1, 3]);
        let b = int_list(&mut host, &[2, 4]);
        term_list(&mut host, &[a, b])
    };
    let got = host
        .call("lists", "merge", &[lists])
        .expect("lists:merge/1 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4]);
}

// ---------------------------------------------------------------------------
// `ordsets` — the n-ary union / intersection
// ---------------------------------------------------------------------------

/// `ordsets:union/1` and `ordsets:intersection/1` — the n-ary forms.
///
/// Families: lists, cross-module `ordsets` → `lists` calls.
#[test]
fn stdlib_ordsets_n_ary_operations() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:union([[1,2],[2,3]])]),halt().'
    //         [1,2,3]
    let sets = {
        let a = int_list(&mut host, &[1, 2]);
        let b = int_list(&mut host, &[2, 3]);
        term_list(&mut host, &[a, b])
    };
    let got = host
        .call("ordsets", "union", &[sets])
        .expect("ordsets:union/1 must run");
    assert_int_list(&host, got, &[1, 2, 3]);

    // oracle: erl -noshell -eval 'io:format("~p~n",[ordsets:intersection([[1,2],[2,3]])]),halt().'
    //         [2]
    let sets = {
        let a = int_list(&mut host, &[1, 2]);
        let b = int_list(&mut host, &[2, 3]);
        term_list(&mut host, &[a, b])
    };
    let got = host
        .call("ordsets", "intersection", &[sets])
        .expect("ordsets:intersection/1 must run");
    assert_int_list(&host, got, &[2]);
}

// ---------------------------------------------------------------------------
// Robustness guard (does not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn the_located_stdlib_is_a_usable_stdlib() {
    // Mirrors `tests/stdlib_text.rs`: a genuinely independent check of the skip
    // guard's *input* — when the locator claims a stdlib, that directory must
    // actually hold the real `unicode.beam` this suite runs.
    let Some(dir) = stdlib_ebin() else {
        // No locatable stdlib at all: a genuine absent-optional-dependency skip.
        eprintln!("{}", skip_reason().unwrap_or_default());
        return;
    };
    assert!(
        dir.join("unicode.beam").is_file(),
        "the located oracle stdlib {dir:?} must hold the real `unicode.beam`"
    );
    assert!(
        skip_reason().is_none(),
        "a located, usable stdlib must not produce a skip reason"
    );
}
