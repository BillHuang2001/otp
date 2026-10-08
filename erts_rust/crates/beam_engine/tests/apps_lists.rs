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

//! **Per-application execution breadth** — the widened oracle-checked sweep over
//! OTP applications *beyond* `stdlib`/`kernel`/`compiler`.
//!
//! `tests/stdlib_lists.rs` / `tests/stdlib_sweep.rs` prove the engine runs real
//! `stdlib`/`kernel`/`compiler` modules. This file pushes the same evidence into
//! the **rest of the installed OTP**: `syntax_tools` (`erl_syntax`), `xmerl`
//! (`xmerl_ucs`/`xmerl_lib`), `inets` (`httpd_util`), `edoc` (`edoc_lib`),
//! `eunit` (`eunit_lib`), `dialyzer` (`dialyzer_utils`), `asn1`
//! (`asn1ct_pretty_format`), `snmp` (`snmp_misc`/`snmp_pdus`) and `parsetools`
//! (`leex`/`yecc`).
//!
//! The targets are deliberately **pure-Erlang** functions: they build/read
//! tuples, lists, binaries and integers without needing a live process, a
//! mailbox, an `io` device or a code server — so an execution here is real
//! evidence about the *engine*, not about an absent host facility.
//!
//! # Ground truth
//!
//! Every expected value is the **system Erlang** result (the differential
//! oracle), reproduced with `erl -noshell -eval …` and quoted next to each
//! assertion. Nothing is guessed. The applications are located at **test time**
//! (never a baked-in path); every test **skips loudly** when the oracle OTP
//! install is unavailable.
//!
//! # Skip-vs-assert policy
//!
//! * A target the oracle agrees on **must execute** here; a typed error where the
//!   oracle returns a value FAILS the test.
//! * [`blocked_targets_report_their_typed_error`] is the honest counterpart: for
//!   each target the engine still cannot execute it prints `SKIP: … — <typed
//!   error>` **and asserts the exact typed error**, so a regression that turns a
//!   typed error into a *wrong result* is caught (never a silent pass).

mod support;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    all_otp_ebins, assert_atom, assert_int_list, assert_small, assert_str, assert_term, chars,
    int_list, skip_reason, stdlib_ebin, term_list, term_tuple, ModuleHost,
    PERSISTENT_TERM_CAPABILITY,
};

// ---------------------------------------------------------------------------
// Host + helpers
// ---------------------------------------------------------------------------

/// Skips the calling test (loudly) when the oracle OTP install is unavailable;
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

/// A host whose primary `ebin` is `stdlib` and whose **search path is every
/// installed OTP application** — so a call into `syntax_tools`, `xmerl`, `snmp`,
/// … resolves by name, exactly as a real runtime's code path would.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    if let Some(apps) = all_otp_ebins() {
        for app in apps {
            host.add_search_dir(app.dir);
        }
    }
    host
}

/// Runs `module:function/arity` and returns the result (a typed error panics).
fn run(host: &mut ModuleHost, module: &str, function: &str, args: &[Eterm]) -> Eterm {
    host.call(module, function, args)
        .unwrap_or_else(|error| panic!("{module}:{function}/{} failed: {error:?}", args.len()))
}

/// Asserts `got` is the boxed float `want` (compared by its `f64` value).
fn assert_float(got: Eterm, want: f64) {
    let value = terms::float_value(got).unwrap_or_else(|error| panic!("not a float: {error:?}"));
    assert_eq!(value, want, "got {value}, want {want}");
}

// ---------------------------------------------------------------------------
// syntax_tools — `erl_syntax` (the syntax-tree constructors/accessors)
// ---------------------------------------------------------------------------

/// `erl_syntax` builds and reads `#tree{}` records — pure tuple/list construction
/// plus the record-accessor clauses, with the atom/integer/float value accessors
/// (`atom_value/1`, `integer_value/1`, `float_value/1`, `concrete/1`).
#[test]
fn apps_syntax_tools_erl_syntax_tree_round_trip() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl_syntax:type(erl_syntax:atom(foo)) = atom
    let foo = host.intern("foo");
    let tree = run(&mut host, "erl_syntax", "atom", &[foo]);
    let ty = run(&mut host, "erl_syntax", "type", &[tree]);
    assert_atom(&host, ty, "atom");

    // oracle: erl_syntax:atom_value(erl_syntax:atom(foo)) = foo
    let value = run(&mut host, "erl_syntax", "atom_value", &[tree]);
    assert_atom(&host, value, "foo");

    // `erl_syntax:atom_literal(erl_syntax:atom(foo))` would be `"foo"` in the
    // oracle, but the rendering goes through `io_lib`'s atom writer, which asks
    // `erl_features:keywords/0` → `persistent_term:get/2` — a **NIF** the host
    // cannot load. It is pinned in
    // [`blocked_targets_report_their_typed_error`] instead of being asserted here.

    // oracle: erl_syntax:type(erl_syntax:integer(42)) = integer
    let forty_two = make_small(42);
    let itree = run(&mut host, "erl_syntax", "integer", &[forty_two]);
    let ty = run(&mut host, "erl_syntax", "type", &[itree]);
    assert_atom(&host, ty, "integer");

    // oracle: erl_syntax:integer_value(erl_syntax:integer(42)) = 42
    let value = run(&mut host, "erl_syntax", "integer_value", &[itree]);
    assert_small(&host, value, 42);

    // oracle: erl_syntax:concrete(erl_syntax:integer(42)) = 42
    let value = run(&mut host, "erl_syntax", "concrete", &[itree]);
    assert_small(&host, value, 42);

    // oracle: erl_syntax:float_value(erl_syntax:float(1.5)) = 1.5
    let one_and_a_half = terms::make_float(&mut host, 1.5).expect("a float term");
    let ftree = run(&mut host, "erl_syntax", "float", &[one_and_a_half]);
    let value = run(&mut host, "erl_syntax", "float_value", &[ftree]);
    assert_float(value, 1.5);

    // oracle: erl_syntax:type(erl_syntax:list([erl_syntax:integer(1)])) = list
    let one = run(&mut host, "erl_syntax", "integer", &[make_small(1)]);
    let elements = term_list(&mut host, &[one]);
    let ltree = run(&mut host, "erl_syntax", "list", &[elements]);
    let ty = run(&mut host, "erl_syntax", "type", &[ltree]);
    assert_atom(&host, ty, "list");
}

/// `erl_syntax:variable_literal/1` lower-cases a variable name and
/// `erl_syntax:string/1`/`concrete/1` round-trip a string through a `#tree{}`.
#[test]
fn apps_syntax_tools_erl_syntax_string_and_variable() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl_syntax:variable_literal(erl_syntax:variable(v)) = "v"
    let v = host.intern("v");
    let tree = run(&mut host, "erl_syntax", "variable", &[v]);
    let literal = run(&mut host, "erl_syntax", "variable_literal", &[tree]);
    assert_str(&host, literal, "v");

    // oracle: erl_syntax:string("abc") = {tree,string,{attr,0,[],none},"abc"}
    let abc = chars(&mut host, "abc");
    let stree = run(&mut host, "erl_syntax", "string", &[abc]);
    let attr = host.intern("attr");
    let none = host.intern("none");
    let no_anns = term_list(&mut host, &[]);
    let attr0 = term_tuple(&mut host, &[attr, make_small(0), no_anns, none]);
    let tree_atom = host.intern("tree");
    let string_atom = host.intern("string");
    let abc = chars(&mut host, "abc");
    let expected = term_tuple(&mut host, &[tree_atom, string_atom, attr0, abc]);
    assert_term(&host, stree, expected);

    // oracle: erl_syntax:type(erl_syntax:string("abc")) = string
    let ty = run(&mut host, "erl_syntax", "type", &[stree]);
    assert_atom(&host, ty, "string");

    // oracle: erl_syntax:concrete(erl_syntax:string("abc")) = "abc"
    let concrete = run(&mut host, "erl_syntax", "concrete", &[stree]);
    assert_str(&host, concrete, "abc");
}

// ---------------------------------------------------------------------------
// xmerl — `xmerl_ucs` (character-set conversion) + `xmerl_lib` (name tests)
// ---------------------------------------------------------------------------

/// `xmerl_ucs` converts code points to/from their UTF-8 byte sequences — a pure
/// guarded bit/integer computation over the `bs_*`/integer opcodes.
#[test]
fn apps_xmerl_ucs_utf8_conversion() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: xmerl_ucs:to_utf8(65) = "A"
    let got = run(&mut host, "xmerl_ucs", "to_utf8", &[make_small(65)]);
    assert_int_list(&host, got, &[65]);

    // oracle: xmerl_ucs:to_utf8(233) = "Ã©"  (the two UTF-8 bytes 195,169)
    let got = run(&mut host, "xmerl_ucs", "to_utf8", &[make_small(233)]);
    assert_int_list(&host, got, &[195, 169]);

    // oracle: xmerl_ucs:from_utf8([65]) = "A"
    let bytes = int_list(&mut host, &[65]);
    let got = run(&mut host, "xmerl_ucs", "from_utf8", &[bytes]);
    assert_int_list(&host, got, &[65]);

    // oracle: xmerl_ucs:from_utf8([195,169]) = "é"
    let bytes = int_list(&mut host, &[195, 169]);
    let got = run(&mut host, "xmerl_ucs", "from_utf8", &[bytes]);
    assert_int_list(&host, got, &[233]);
}

/// `xmerl_lib`'s character/name predicates and `to_lower/1` — long guarded
/// `case`/comparison chains over code points.
#[test]
fn apps_xmerl_lib_name_and_char_tests() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: xmerl_ucs:is_ascii(65) = true
    let got = run(&mut host, "xmerl_ucs", "is_ascii", &[make_small(65)]);
    assert_atom(&host, got, "true");

    // oracle: xmerl_ucs:is_latin1(233) = true
    let got = run(&mut host, "xmerl_ucs", "is_latin1", &[make_small(233)]);
    assert_atom(&host, got, "true");

    // oracle: xmerl_lib:is_letter(97) = true
    let got = run(&mut host, "xmerl_lib", "is_letter", &[make_small(97)]);
    assert_atom(&host, got, "true");

    // oracle: xmerl_lib:is_letter(48) = false
    let got = run(&mut host, "xmerl_lib", "is_letter", &[make_small(48)]);
    assert_atom(&host, got, "false");

    // oracle: xmerl_lib:is_name("abc") = true
    let name = chars(&mut host, "abc");
    let got = run(&mut host, "xmerl_lib", "is_name", &[name]);
    assert_atom(&host, got, "true");

    // oracle: xmerl_lib:is_char(60) = true
    let got = run(&mut host, "xmerl_lib", "is_char", &[make_small(60)]);
    assert_atom(&host, got, "true");

    // oracle: xmerl_lib:to_lower("ABC") = "abc"
    let upper = chars(&mut host, "ABC");
    let got = run(&mut host, "xmerl_lib", "to_lower", &[upper]);
    assert_str(&host, got, "abc");
}

// ---------------------------------------------------------------------------
// inets — `httpd_util` (pure request/date/name helpers)
// ---------------------------------------------------------------------------

/// `httpd_util`'s month/day/reason-phrase lookups and the path/duplicate helpers.
#[test]
fn apps_inets_httpd_util_pure_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: httpd_util:month(1) = "Jan"
    let got = run(&mut host, "httpd_util", "month", &[make_small(1)]);
    assert_str(&host, got, "Jan");

    // oracle: httpd_util:month(12) = "Dec"
    let got = run(&mut host, "httpd_util", "month", &[make_small(12)]);
    assert_str(&host, got, "Dec");

    // oracle: httpd_util:day(1) = "Mon"
    let got = run(&mut host, "httpd_util", "day", &[make_small(1)]);
    assert_str(&host, got, "Mon");

    // oracle: httpd_util:day(7) = "Sun"
    let got = run(&mut host, "httpd_util", "day", &[make_small(7)]);
    assert_str(&host, got, "Sun");

    // oracle: httpd_util:reason_phrase(200) = "OK"
    let got = run(&mut host, "httpd_util", "reason_phrase", &[make_small(200)]);
    assert_str(&host, got, "OK");

    // oracle: httpd_util:reason_phrase(404) = "Object Not Found"
    let got = run(&mut host, "httpd_util", "reason_phrase", &[make_small(404)]);
    assert_str(&host, got, "Object Not Found");

    // oracle: httpd_util:reason_phrase(500) = "Internal Server Error"
    let got = run(&mut host, "httpd_util", "reason_phrase", &[make_small(500)]);
    assert_str(&host, got, "Internal Server Error");

    // oracle: httpd_util:collapse_slashes("/a//b") = "/a/b"
    let path = chars(&mut host, "/a//b");
    let got = run(&mut host, "httpd_util", "collapse_slashes", &[path]);
    assert_str(&host, got, "/a/b");

    // oracle: httpd_util:collapse_slashes("a//b//c") = "a/b/c"
    let path = chars(&mut host, "a//b//c");
    let got = run(&mut host, "httpd_util", "collapse_slashes", &[path]);
    assert_str(&host, got, "a/b/c");

    // oracle: httpd_util:uniq([1,1,2]) = [1,2]
    let list = int_list(&mut host, &[1, 1, 2]);
    let got = run(&mut host, "httpd_util", "uniq", &[list]);
    assert_int_list(&host, got, &[1, 2]);

    // `httpd_util:module_validate(httpd_file)` would be `ok` in the oracle, but it
    // asks `code:which/1` — a **code-server** query — so it is pinned in
    // [`blocked_targets_report_their_typed_error`] rather than asserted here.
}

// ---------------------------------------------------------------------------
// edoc — `edoc_lib` (pure string/URI helpers)
// ---------------------------------------------------------------------------

/// `edoc_lib`'s URI escaping, character counting and space test — list recursion
/// plus a code-point loop.
#[test]
fn apps_edoc_lib_string_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: edoc_lib:escape_uri("a b") = "a%20b"
    let uri = chars(&mut host, "a b");
    let got = run(&mut host, "edoc_lib", "escape_uri", &[uri]);
    assert_str(&host, got, "a%20b");

    // oracle: edoc_lib:escape_uri("/a/b") = "%2fa%2fb"
    let uri = chars(&mut host, "/a/b");
    let got = run(&mut host, "edoc_lib", "escape_uri", &[uri]);
    assert_str(&host, got, "%2fa%2fb");

    // oracle: edoc_lib:count(97, "banana") = 3   ($a)
    let banana = chars(&mut host, "banana");
    let got = run(&mut host, "edoc_lib", "count", &[make_small(97), banana]);
    assert_small(&host, got, 3);

    // oracle: edoc_lib:count(98, "banana") = 1   ($b)
    let banana = chars(&mut host, "banana");
    let got = run(&mut host, "edoc_lib", "count", &[make_small(98), banana]);
    assert_small(&host, got, 1);

    // oracle: edoc_lib:is_space("x") = false
    let x = chars(&mut host, "x");
    let got = run(&mut host, "edoc_lib", "is_space", &[x]);
    assert_atom(&host, got, "false");

    // oracle: edoc_lib:is_space("") = true
    let empty = chars(&mut host, "");
    let got = run(&mut host, "edoc_lib", "is_space", &[empty]);
    assert_atom(&host, got, "true");

    // oracle: edoc_lib:get_first_sentence("Hello. World.") = "Hello. World."
    let text = chars(&mut host, "Hello. World.");
    let got = run(&mut host, "edoc_lib", "get_first_sentence", &[text]);
    assert_str(&host, got, "Hello. World.");
}

// ---------------------------------------------------------------------------
// eunit — `eunit_lib` (pure list/trie helpers)
// ---------------------------------------------------------------------------

/// `eunit_lib`'s duplicate-removal, node splitting and the (pure) trie
/// constructor — the shape `eunit`'s own selectors/matchers build on.
#[test]
fn apps_eunit_lib_pure_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: eunit_lib:uniq([1,1,2,3,3]) = [1,2,3]
    let list = int_list(&mut host, &[1, 1, 2, 3, 3]);
    let got = run(&mut host, "eunit_lib", "uniq", &[list]);
    assert_int_list(&host, got, &[1, 2, 3]);

    // oracle: eunit_lib:is_not_test(x) = false
    let x = host.intern("x");
    let got = run(&mut host, "eunit_lib", "is_not_test", &[x]);
    assert_atom(&host, got, "false");

    // oracle: eunit_lib:split_node('a@b') = {a,b}
    let node = host.intern("a@b");
    let got = run(&mut host, "eunit_lib", "split_node", &[node]);
    let a = host.intern("a");
    let b = host.intern("b");
    let expected = term_tuple(&mut host, &[a, b]);
    assert_term(&host, got, expected);

    // oracle: eunit_lib:trie_new() = {0,nil}
    let got = run(&mut host, "eunit_lib", "trie_new", &[]);
    let nil = host.intern("nil");
    let zero = make_small(0);
    let expected = term_tuple(&mut host, &[zero, nil]);
    assert_term(&host, got, expected);
}

// ---------------------------------------------------------------------------
// dialyzer — `dialyzer_utils` (pure option/type helpers)
// ---------------------------------------------------------------------------

/// `dialyzer_utils`'s constant option list and the `family/1` type-key helper.
#[test]
fn apps_dialyzer_utils_options_and_family() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: dialyzer_utils:family([]) = []
    let empty = term_list(&mut host, &[]);
    let got = run(&mut host, "dialyzer_utils", "family", &[empty]);
    assert_int_list(&host, got, &[]);

    // oracle: dialyzer_utils:src_compiler_opts() =
    //   [no_copt,to_core,binary,return_errors,no_inline,strict_record_tests,
    //    dialyzer,no_spawn_compiler_process]
    let got = run(&mut host, "dialyzer_utils", "src_compiler_opts", &[]);
    let elements = terms::list_elements(got).expect("an option list");
    let names: Vec<String> = elements
        .iter()
        .map(|&element| {
            host.atom_table()
                .name(beam_term::atom_val(element))
                .unwrap_or_else(|| panic!("not an atom: {element:?}"))
                .to_string()
        })
        .collect();
    assert_eq!(
        names,
        vec![
            "no_copt",
            "to_core",
            "binary",
            "return_errors",
            "no_inline",
            "strict_record_tests",
            "dialyzer",
            "no_spawn_compiler_process",
        ],
        "dialyzer_utils:src_compiler_opts/0 must equal the oracle"
    );
}

// ---------------------------------------------------------------------------
// asn1 — `asn1ct_pretty_format` (pure term formatter)
// ---------------------------------------------------------------------------

/// `asn1ct_pretty_format:term/1` renders an ASN.1 value as Erlang source — pure
/// tuple/list character building (integers mixed into strings).
///
/// Only the **integer** form executes here: every form that renders an *atom* goes
/// through `io_lib`'s atom writer, which asks `erl_scan:reserved_word/1` →
/// `erl_features:keywords/0` → `persistent_term:get/2` — a NIF this host cannot
/// load. The atom-bearing forms are pinned in
/// [`blocked_targets_report_their_typed_error`].
#[test]
fn apps_asn1_pretty_format_term() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: asn1ct_pretty_format:term(42) = "42"
    let got = run(&mut host, "asn1ct_pretty_format", "term", &[make_small(42)]);
    assert_str(&host, got, "42");
}

// ---------------------------------------------------------------------------
// snmp — `snmp_misc` / `snmp_pdus` (pure OID/string/BER-length helpers)
// ---------------------------------------------------------------------------

/// `snmp_misc`'s OID/string predicates and `str_xor/2` (a `bxor` list loop).
#[test]
fn apps_snmp_misc_predicates_and_str_xor() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: snmp_misc:is_oid([1,3,6]) = true
    let oid = int_list(&mut host, &[1, 3, 6]);
    let got = run(&mut host, "snmp_misc", "is_oid", &[oid]);
    assert_atom(&host, got, "true");

    // oracle: snmp_misc:is_oid([0]) = true
    let oid = int_list(&mut host, &[0]);
    let got = run(&mut host, "snmp_misc", "is_oid", &[oid]);
    assert_atom(&host, got, "true");

    // oracle: snmp_misc:is_oid(foo) = false
    let foo = host.intern("foo");
    let got = run(&mut host, "snmp_misc", "is_oid", &[foo]);
    assert_atom(&host, got, "false");

    // oracle: snmp_misc:is_string("abc") = true
    let abc = chars(&mut host, "abc");
    let got = run(&mut host, "snmp_misc", "is_string", &[abc]);
    assert_atom(&host, got, "true");

    // oracle: snmp_misc:is_string([1,2,300]) = false
    let list = int_list(&mut host, &[1, 2, 300]);
    let got = run(&mut host, "snmp_misc", "is_string", &[list]);
    assert_atom(&host, got, "false");

    // oracle: snmp_misc:str_xor("abc","def") = [5,7,5]
    let a = chars(&mut host, "abc");
    let b = chars(&mut host, "def");
    let got = run(&mut host, "snmp_misc", "str_xor", &[a, b]);
    assert_int_list(&host, got, &[5, 7, 5]);

    // oracle: snmp_misc:str_xor("ab","cd") = [2,6]
    let a = chars(&mut host, "ab");
    let b = chars(&mut host, "cd");
    let got = run(&mut host, "snmp_misc", "str_xor", &[a, b]);
    assert_int_list(&host, got, &[2, 6]);
}

/// `snmp_pdus:get_encoded_length/1` — the BER length-of-length computation
/// (`length(elength(N))`), pure integer/list construction.
#[test]
fn apps_snmp_pdus_ber_length() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: snmp_pdus:get_encoded_length(0) = 1
    let got = run(&mut host, "snmp_pdus", "get_encoded_length", &[make_small(0)]);
    assert_small(&host, got, 1);

    // oracle: snmp_pdus:get_encoded_length(5) = 1
    let got = run(&mut host, "snmp_pdus", "get_encoded_length", &[make_small(5)]);
    assert_small(&host, got, 1);

    // oracle: snmp_pdus:get_encoded_length(200) = 2
    let got = run(&mut host, "snmp_pdus", "get_encoded_length", &[make_small(200)]);
    assert_small(&host, got, 2);
}

// ---------------------------------------------------------------------------
// parsetools — `leex` / `yecc` (the pure error formatters)
// ---------------------------------------------------------------------------

/// `leex`'s literal error strings and `yecc:format_error/1`'s `io_lib:fwrite/2`
/// path — the latter forces the engine through the real `io_lib` module.
#[test]
fn apps_parsetools_error_formatters() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: leex:format_error(missing_defs) = "missing Definitions"
    let reason = host.intern("missing_defs");
    let got = run(&mut host, "leex", "format_error", &[reason]);
    assert_str(&host, got, "missing Definitions");

    // oracle: leex:format_error(empty_rules) = "no rules"
    let reason = host.intern("empty_rules");
    let got = run(&mut host, "leex", "format_error", &[reason]);
    assert_str(&host, got, "no rules");

    // oracle: leex:format_error(bad_rule) = "bad rule"
    let reason = host.intern("bad_rule");
    let got = run(&mut host, "leex", "format_error", &[reason]);
    assert_str(&host, got, "bad rule");

    // oracle: leex:format_error(ignored_characters) = "ignored characters"
    let reason = host.intern("ignored_characters");
    let got = run(&mut host, "leex", "format_error", &[reason]);
    assert_str(&host, got, "ignored characters");

    // oracle: yecc:format_error(bad_declaration) =
    //   "unknown or bad declaration, ignored"
    let reason = host.intern("bad_declaration");
    let got = run(&mut host, "yecc", "format_error", &[reason]);
    assert_str(&host, got, "unknown or bad declaration, ignored");
}

// ---------------------------------------------------------------------------
// Blocked targets (asserted loudly, never silently passed)
// ---------------------------------------------------------------------------

/// Asserts a blocked probe stopped at the host floor's typed BEAM `badarg` (the
/// answer a NIF-stub / code-server chain bottoms out in), printing the host
/// capability the target is missing.
fn assert_blocked_badarg(target: &str, capability: &str, err: &BeamError) {
    println!("SKIP: {target} — needs {capability}: {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `{target}` typed error: {err:?}"
    );
    assert_eq!(
        err.as_atom(),
        Some("badarg"),
        "`{target}` must be the typed BEAM `badarg`"
    );
}

/// Asserts a blocked probe stopped at the host floor's typed `unsupported_opcode`
/// refusal **whose reason names the missing capability** (`capability`, the exact
/// fragment the floor passed to `beam_engine::unsupported`), printing it too.
fn assert_blocked_unsupported(target: &str, capability: &str, err: &BeamError) {
    let expected = format!("unsupported_opcode: {capability}");
    println!("SKIP: {target} — needs {capability}: {err:?}");
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some(expected.as_str()),
            "unexpected `{target}` typed error: {err:?}"
        ),
        other => panic!(
            "`{target}` must raise the typed `unsupported_opcode` refusal naming its \
             missing capability, got {other:?}"
        ),
    }
}

/// Blocks that the host/engine still cannot serve, each asserted to be a **typed**
/// error — never a wrong value and never a hang.
///
/// These are the residual gaps measured while widening the sweep; every one is a
/// *host facility* the engine deliberately does not fake, so the probe pins the
/// exact typed error shape rather than inventing a result:
///
/// * `asn1ct:use_legacy_types/0` / `get_bit_string_format/0` read the **process
///   dictionary** (`erlang:get/1`), which this host does not serve — the real
///   runtime supplies it through `beam_rt`'s `DictHook`, not the engine.
/// * `httpd_util:module_validate/1` asks `code:which/1`, which resolves a module
///   through the **code server** (`code_server:is_loaded/1` → the `ets:lookup/2`
///   NIF). The host serves neither, so the chain stops at the floor's typed
///   `badarg` — the same pin `tests/stdlib_sweep.rs` records for `code:which/1`
///   itself.
/// * `erl_syntax:atom_literal/1` and `asn1ct_pretty_format:term/1` render a term
///   through `io_lib`'s atom writer (`io_lib:write_atom_as_latin1/1`), which asks
///   `erl_scan:reserved_word/1` → `erl_features:keywords/0` →
///   `persistent_term:get/2` — a **NIF**, and this host loads no NIF library, so
///   the reason *names* that missing capability instead of a bare `badarg`.
/// * `httpd_util:create_etag/1` demands a `#file_info{}` **record**, so a plain
///   tuple is the record-semantics `badrecord` failure the engine raises — as a
///   **structured** reason, exactly as the oracle does.
#[test]
fn blocked_targets_report_their_typed_error() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // `asn1ct:use_legacy_types/0` is `get(use_legacy_erlang_types)` — a process-
    // dictionary read (`erlang:get/1`), a host BIF the engine's test host does
    // not serve. oracle: undefined.
    let err = host
        .call("asn1ct", "use_legacy_types", &[])
        .expect_err("`asn1ct:use_legacy_types/0` needs the process dictionary");
    assert_blocked_badarg(
        "asn1ct:use_legacy_types/0",
        "the process dictionary (`erlang:get/1`)",
        &err,
    );

    // `asn1ct:get_bit_string_format/0` is likewise `get(bit_string_format)`.
    // oracle: undefined.
    let err = host
        .call("asn1ct", "get_bit_string_format", &[])
        .expect_err("`asn1ct:get_bit_string_format/0` needs the process dictionary");
    assert_blocked_badarg(
        "asn1ct:get_bit_string_format/0",
        "the process dictionary (`erlang:get/1`)",
        &err,
    );

    // ---- code server -----------------------------------------------------
    // `httpd_util:module_validate/1` is `case code:which(Module) of …`; `code`'s
    // own `which/1` reaches `code_server:is_loaded/1`, which looks the module up in
    // the code server's ETS table (`ets:lookup/2`, a NIF). The host has no code
    // server, so the whole chain is a typed `badarg`.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[httpd_util:module_validate(httpd_file)]),halt().'
    //         ok
    let module = host.intern("httpd_file");
    let err = host
        .call("httpd_util", "module_validate", &[module])
        .expect_err("`httpd_util:module_validate/1` needs `code:which/1` (a code server)");
    assert_blocked_badarg(
        "httpd_util:module_validate/1",
        "`code:which/1` — a live code server over the `ets` NIF surface",
        &err,
    );

    // ---- `io_lib`'s atom writer (a NIF-backed feature lookup) -------------
    // `erl_syntax:atom_literal/1` → `io_lib:write_atom_as_latin1/1`, which quotes
    // the atom via `erl_scan:reserved_word/1` → `erl_features:keywords/0` →
    // `persistent_term:get/2`. `persistent_term` is a **NIF** module (its `.beam`
    // body is the `erlang:nif_error/1` stub), so the host answers with a typed
    // refusal that names that capability.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[erl_syntax:atom_literal(erl_syntax:atom(foo))]),halt().'
    //         "foo"
    let foo = host.intern("foo");
    let tree = run(&mut host, "erl_syntax", "atom", &[foo]);
    let err = host
        .call("erl_syntax", "atom_literal", &[tree])
        .expect_err("`erl_syntax:atom_literal/1` needs the `persistent_term` NIF");
    assert_blocked_unsupported(
        "erl_syntax:atom_literal/1",
        PERSISTENT_TERM_CAPABILITY,
        &err,
    );

    // `asn1ct_pretty_format:term/1` renders every form that contains an **atom**
    // through the same `io_lib` atom writer, so a tuple of atoms and a list of
    // atoms are both blocked by the unloaded `persistent_term` NIF — a *host*
    // facility, not an engine opcode. (`term(42)` executes above.)
    //
    // oracle: asn1ct_pretty_format:term({foo,bar}) = [123,["foo",44,"bar"],125]
    //         asn1ct_pretty_format:term([a,b]) = [91,"a",44,"b",93]
    let bar = host.intern("bar");
    let input = term_tuple(&mut host, &[foo, bar]);
    let err = host
        .call("asn1ct_pretty_format", "term", &[input])
        .expect_err("`asn1ct_pretty_format:term/1` needs the `persistent_term` NIF");
    assert_blocked_unsupported(
        "asn1ct_pretty_format:term/1 ({foo,bar})",
        PERSISTENT_TERM_CAPABILITY,
        &err,
    );

    let a = host.intern("a");
    let b = host.intern("b");
    let input = term_list(&mut host, &[a, b]);
    let err = host
        .call("asn1ct_pretty_format", "term", &[input])
        .expect_err("`asn1ct_pretty_format:term/1` needs the `persistent_term` NIF");
    assert_blocked_unsupported(
        "asn1ct_pretty_format:term/1 ([a,b])",
        PERSISTENT_TERM_CAPABILITY,
        &err,
    );

    // ---- a structured raise reason ---------------------------------------
    // `httpd_util:create_etag/1` matches a `#file_info{}` record, so the plain
    // tuple below is the `badrecord` the engine must raise.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[catch httpd_util:create_etag({x,{2000,1,1},0})]),halt().'
    //         {'EXIT',{{badrecord,{x,{2000,1,1},0}},[…]}}
    //
    // A tuple reason is **structured**: the engine raises a `BeamError::Raise` that
    // *carries the live `{badrecord, Value}` term* in its `live_reason` slot, so
    // `raise.reason.as_atom()` is `None` (the `ExitReason::Structured` sentinel).
    // Asserting a `badrecord` atom here would assert a shape the engine does not
    // produce, so the structured term itself is unwrapped and pinned.
    let date = term_tuple(&mut host, &[make_small(2000), make_small(1), make_small(1)]);
    let name = chars(&mut host, "x");
    let size = make_small(0);
    let bogus = term_tuple(&mut host, &[name, date, size]);
    let err = host
        .call("httpd_util", "create_etag", &[bogus])
        .expect_err("`httpd_util:create_etag/1` needs a `#file_info{}` record");
    println!("SKIP: httpd_util:create_etag/1 — needs a `#file_info{{}}` record: {err:?}");
    let BeamError::Raise(raise) = &err else {
        panic!("`httpd_util:create_etag/1` must raise, got {err:?}");
    };
    assert_eq!(
        raise.class,
        ExceptionClass::Error,
        "the `badrecord` failure is an `error` class raise: {err:?}"
    );
    assert!(
        raise.reason.as_atom().is_none(),
        "the `{{badrecord, _}}` reason is structured, not a bare atom: {err:?}"
    );
    let reason = *raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .unwrap_or_else(|| panic!("the raise must carry the live reason term: {err:?}"));
    assert_eq!(
        terms::tuple_arity(reason).expect("the live reason is a tuple"),
        2,
        "the live reason must be `{{badrecord, Value}}`: {reason:?}"
    );
    assert_atom(
        &host,
        terms::tuple_element(reason, 0).expect("the reason tag"),
        "badrecord",
    );
    assert_term(
        &host,
        terms::tuple_element(reason, 1).expect("the offending term"),
        bogus,
    );
}
