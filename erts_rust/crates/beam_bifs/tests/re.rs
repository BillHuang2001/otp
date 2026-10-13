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

//! Integration tests for the `re:*` BIF family, driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) + [`BifContext`] — the
//! exact public seam `beam_rt` dispatches on (`default_registry()`, no
//! crate-internal API). The eight `re` keys (`version/0`, `compile/1,2`,
//! `import/1`, `inspect/2`, `internal_run/4`, `run/2,3`) are served for real by
//! the in-crate engine, so their observable results are pinned here just like any
//! other stdlib-visible BIF.
//!
//! Two layers, as the crate's `src/re/CONTEXT.md` prescribes:
//!
//! 1. **Committed goldens** — every row is a self-contained assertion that runs
//!    with **no oracle present**. Each expectation is the literal output the
//!    system oracle (OTP 29.1.1 / erts-17.1) produced for the equivalent
//!    `re:*` call, rendered with `io:format("~p", [Result])` and pinned once:
//!
//!    ```text
//!    erl -noshell -eval 'io:format("~p~n",[re:run("xabcy","b")]),halt(0).'
//!    % {match,[{2,1}]}
//!    ```
//!
//! 2. **A live-oracle cross-check** of the core cases
//!    ([`core_cases_cross_check_the_live_oracle`]), which re-derives them from
//!    `$ERTS_ORACLE`/`erl` and **skips loudly** when no oracle is on `PATH` — the
//!    `tests/checksum.rs` / `tests/calendar.rs` pattern.
//!
//! # What is compared (and what is not)
//!
//! The compiled `Payload` is **our own** self-contained binary image, not PCRE2's
//! pattern object, so a `{re_pattern, …}` term is never compared against the
//! oracle's (the oracle prints `#Ref<…>`); only the *observable* results
//! (`{match, …}` / `nomatch` / `match` / `{error, …}` / the `inspect` item / the
//! `version/0` binary) are pinned. Every case is built through the public API —
//! `BifContext` for the terms, `beam_codec::term` for binaries, `io:format("~p")`
//! for the rendering — and no optional host seam is installed (the engine needs
//! none), so an unseeded atom or an under-sized heap surfaces as a real failure.
//!
//! Bare BIF names are **not** auto-imported under `-eval`, so the oracle
//! expressions fully qualify (`re:run(...)`), and each `erl` eval ends with an
//! explicit `halt(0).`

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_codec::term as codec_term;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{
    compare::eq, is_list, is_nil, make_atom, make_small, signed_value, AtomTable, Eterm,
};

/// Asserts a `re:*` call's result renders (under `~p`) exactly as `$expected`.
///
/// The call and the [`Fixture::show`] render are sequenced through this macro
/// because both borrow the fixture mutably; a plain helper taking `&mut Fixture`
/// plus an already-evaluated `Result` could not express both.
macro_rules! renders {
    ($fx:ident, $call:expr, $expected:expr) => {{
        let result = $call;
        let term = result.unwrap_or_else(|e| panic!("{} => unexpected error {e:?}", $expected));
        let got = $fx.show(term);
        assert_eq!(got, $expected, "a re result rendered differently");
    }};
}

/// A registry + heap + seeded atom table the tests drive BIF calls through.
///
/// No optional seam is installed: the `re` engine needs none. The seeded atoms are
/// the ones the engine interns or compares against on the paths exercised here
/// (`match` is read with `ctx.atom`, so it *must* be seeded; the option atoms are
/// looked up by name while parsing). Without an `AtomInterner` a missing name is
/// `TermNotFound`, so an unseeded atom fails loudly rather than silently.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed([
            // Result and pattern-tuple vocabulary.
            "ok",
            "error",
            "match",
            "nomatch",
            "match_limit",
            "compile",
            "true",
            "false",
            "re_pattern",
            "re_exported_pattern",
            // Compile options.
            "unicode",
            "newline",
            "crlf",
            "lf",
            "caseless",
            "dotall",
            "multiline",
            "extended",
            "dupnames",
            "export",
            // Run options.
            "global",
            "capture",
            "all",
            "all_but_first",
            "all_names",
            "first",
            "none",
            "index",
            "list",
            "binary",
            "offset",
            "anchored",
            "notbol",
            "notempty",
            "report_errors",
            // The `inspect/2` item.
            "namelist",
        ]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 20).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function(Args)` through the real registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// Calls `re:function(Args)` through the real registry.
    fn ok(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        self.call("re", function, args)
    }

    /// A seeded atom, as a term.
    fn atom(&self, name: &str) -> Eterm {
        make_atom(
            self.atoms
                .lookup(name)
                .unwrap_or_else(|| panic!("`{name}` must be seeded in the fixture")),
        )
    }

    /// Whether `term` is the seeded atom `name`.
    fn is_atom(&self, term: Eterm, name: &str) -> bool {
        self.atoms
            .lookup(name)
            .is_some_and(|index| eq(term, make_atom(index), &self.atoms))
    }

    /// Builds the on-heap binary `<<bytes>>`.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        codec_term::make_binary(&mut self.heap, bytes).expect("the binary fits")
    }

    /// A charlist of `s`'s codepoints (the oracle's string notation).
    fn charlist(&mut self, s: &str) -> Eterm {
        let elems: Vec<Eterm> = s
            .chars()
            .map(|c| make_small(i64::from(u32::from(c))))
            .collect();
        self.list(&elems)
    }

    /// A proper list of `items`.
    fn list(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.list(items).expect("the list fits")
    }

    /// A tuple of `items`.
    fn tuple(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.tuple(items).expect("the tuple fits")
    }

    /// The `i`th element of the tuple `term`.
    fn elem(&self, term: Eterm, i: usize) -> Eterm {
        codec_term::tuple_element(term, i).expect("a tuple element")
    }

    /// The arity of the tuple `term`.
    fn arity(&self, term: Eterm) -> usize {
        codec_term::tuple_arity(term).expect("a tuple")
    }

    // -- Option-term builders (the shapes the oracle's option grammar uses) ----

    /// `{capture, Value}`.
    fn capture2(&mut self, value: Eterm) -> Eterm {
        let capture = self.atom("capture");
        self.tuple(&[capture, value])
    }

    /// `{capture, Value, Type}`.
    fn capture3(&mut self, value: Eterm, ty: &str) -> Eterm {
        let capture = self.atom("capture");
        let ty = self.atom(ty);
        self.tuple(&[capture, value, ty])
    }

    /// `{offset, N}`.
    fn offset(&mut self, n: i64) -> Eterm {
        let tag = self.atom("offset");
        self.tuple(&[tag, make_small(n)])
    }

    /// `{match_limit, N}`.
    fn match_limit(&mut self, n: i64) -> Eterm {
        let tag = self.atom("match_limit");
        self.tuple(&[tag, make_small(n)])
    }

    // -- Call builders --------------------------------------------------------

    /// `re:run/2` on a charlist subject and a binary pattern.
    fn run2(&mut self, subject: &str, pattern: &str) -> Result<Eterm> {
        let subject = self.charlist(subject);
        let pattern = self.binary(pattern.as_bytes());
        self.ok("run", &[subject, pattern])
    }

    /// `re:run/3` on a charlist subject and a binary pattern.
    fn run3(&mut self, subject: &str, pattern: &str, opts: &[Eterm]) -> Result<Eterm> {
        let subject = self.charlist(subject);
        let pattern = self.binary(pattern.as_bytes());
        let opts = self.list(opts);
        self.ok("run", &[subject, pattern, opts])
    }

    /// `re:run/2,3` on an already-built RE (a precompiled `{re_pattern, …}`).
    fn run_with(&mut self, subject: &str, re: Eterm, opts: &[Eterm]) -> Result<Eterm> {
        let subject = self.charlist(subject);
        if opts.is_empty() {
            self.ok("run", &[subject, re])
        } else {
            let opts = self.list(opts);
            self.ok("run", &[subject, re, opts])
        }
    }

    /// `re:compile/1`.
    fn compile1(&mut self, pattern: &str) -> Result<Eterm> {
        let pattern = self.binary(pattern.as_bytes());
        self.ok("compile", &[pattern])
    }

    /// `re:compile/2`.
    fn compile2(&mut self, pattern: &str, opts: &[Eterm]) -> Result<Eterm> {
        let pattern = self.binary(pattern.as_bytes());
        let opts = self.list(opts);
        self.ok("compile", &[pattern, opts])
    }

    /// `re:internal_run/4`.
    fn internal_run(
        &mut self,
        subject: &str,
        re: Eterm,
        opts: &[Eterm],
        first: bool,
    ) -> Result<Eterm> {
        let subject = self.charlist(subject);
        let opts = self.list(opts);
        let first = self.atom(if first { "true" } else { "false" });
        self.ok("internal_run", &[subject, re, opts, first])
    }

    /// `re:inspect/2` with the `namelist` item.
    fn inspect_namelist(&mut self, mp: Eterm) -> Result<Eterm> {
        let item = self.atom("namelist");
        self.ok("inspect", &[mp, item])
    }

    /// Renders `term` with `io:format("~p", [Term])` (the oracle's shape).
    ///
    /// `~p` — not `~w` — is used because it applies `io_lib`'s printable-string
    /// rule, so a captured `"bc"` renders as `"bc"` and not `[98,99]`, matching the
    /// goldens below.
    fn show(&mut self, term: Eterm) -> String {
        let mut sink = BytesSink::new();
        {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0))
                .with_output(&mut sink);
            let format = bytes_list(&mut ctx, b"~p");
            let args = ctx.list(&[term]).expect("a one-element argument list fits");
            self.registry
                .call("io", "format", 2, &mut ctx, &[format, args])
                .expect("io:format(\"~p\", [Term]) renders the result");
        }
        String::from_utf8(sink.contents().to_vec()).expect("the render is ASCII")
    }
}

/// A proper list of the byte-sized integers of `bytes`.
fn bytes_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes.iter().map(|&b| make_small(i64::from(b))).collect();
    ctx.list(&elems).expect("a short list fits")
}

/// The bytes of a proper list of small integers (an Erlang charlist).
fn charlist_bytes(term: Eterm) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = codec_term::cons_head(cur).expect("a cons head");
        out.push(u8::try_from(signed_value(head)).expect("a byte"));
        cur = codec_term::cons_tail(cur).expect("a cons tail");
    }
    assert!(is_nil(cur), "the charlist must be proper");
    out
}

/// Asserts the call failed with `badarg`.
fn assert_badarg(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected badarg, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn the_eight_re_bifs_are_registered() {
    // `bif.tab` declares exactly these eight `re` keys; all are real
    // implementations (not `RuntimeHooks` seam keys).
    let reg = default_registry();
    for (function, arity) in [
        ("version", 0u32),
        ("compile", 1),
        ("compile", 2),
        ("import", 1),
        ("inspect", 2),
        ("internal_run", 4),
        ("run", 2),
        ("run", 3),
    ] {
        assert!(
            reg.contains("re", function, arity),
            "re:{function}/{arity} must be registered"
        );
    }
    // There is no `re:run/1`, `re:compile/0` or `re:version/1`.
    assert!(!reg.contains("re", "run", 1));
    assert!(!reg.contains("re", "compile", 0));
    assert!(!reg.contains("re", "version", 1));
}

// ---------------------------------------------------------------------------
// (a) Committed goldens — no oracle needed
// ---------------------------------------------------------------------------

#[test]
fn capture_shapes_match_the_oracle_goldens() {
    let mut fx = Fixture::new();

    // The default capture for `run/2,3` is `{capture,all,index}`: group 0 (the
    // whole match) first, then every group, flat in a non-global run.
    renders!(fx, fx.run2("xabcy", "b"), "{match,[{2,1}]}");
    renders!(
        fx,
        fx.run2("xabcy", "(b)(c)"),
        "{match,[{2,2},{2,1},{3,1}]}"
    );

    // An empty capture set is the bare `match` atom — `{capture,none}` and the
    // empty list `[]` are the same specification.
    let none = fx.atom("none");
    let spec = fx.capture2(none);
    renders!(fx, fx.run3("xabcy", "b", &[spec]), "match");
    let empty = fx.list(&[]);
    let spec = fx.capture3(empty, "index");
    renders!(fx, fx.run3("xabcy", "(b)(c)", &[spec]), "match");

    // `global` nests one capture list per match.
    let global = fx.atom("global");
    renders!(
        fx,
        fx.run3("abab", "a", &[global]),
        "{match,[[{0,1}],[{2,1}]]}"
    );
    let global = fx.atom("global");
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(
        fx,
        fx.run3("abab", "(a)(b)", &[global, spec]),
        "{match,[[{0,2},{0,1},{1,1}],[{2,2},{2,1},{3,1}]]}"
    );

    // The three value forms of a `{capture,all,_}`: `list` charlists, `binary`
    // binaries, `index` `{Start,Len}` pairs.
    let all = fx.atom("all");
    let spec = fx.capture3(all, "list");
    renders!(
        fx,
        fx.run3("xabcy", "(b)(c)", &[spec]),
        r#"{match,["bc","b","c"]}"#
    );
    let all = fx.atom("all");
    let spec = fx.capture3(all, "binary");
    renders!(
        fx,
        fx.run3("xabcy", "(b)(c)", &[spec]),
        r#"{match,[<<"bc">>,<<"b">>,<<"c">>]}"#
    );
    // `all_but_first` drops group 0; `first` keeps only group 0.
    let abf = fx.atom("all_but_first");
    let spec = fx.capture3(abf, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(b)(c)", &[spec]),
        "{match,[{2,1},{3,1}]}"
    );
    let first = fx.atom("first");
    let spec = fx.capture3(first, "list");
    renders!(fx, fx.run3("xabcy", "(b)(c)", &[spec]), r#"{match,["bc"]}"#);

    // An explicit id list renders in the requested order, group 0 included or not.
    let ids = fx.list(&[make_small(2), make_small(1)]);
    let spec = fx.capture3(ids, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(b)(c)", &[spec]),
        "{match,[{3,1},{2,1}]}"
    );
    let ids = fx.list(&[make_small(0), make_small(1)]);
    let spec = fx.capture3(ids, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(b)(c)", &[spec]),
        "{match,[{2,2},{2,1}]}"
    );

    // A non-capturing group is not in the id list; a group that does not
    // participate renders `{-1,0}` / `[]` / `<<>>`.
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(?:b)(c)", &[spec]),
        "{match,[{2,2},{3,1}]}"
    );
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(z)?(b)", &[spec]),
        "{match,[{2,1},{-1,0},{2,1}]}"
    );
    let all = fx.atom("all");
    let spec = fx.capture3(all, "list");
    renders!(
        fx,
        fx.run3("xabcy", "(z)?(b)", &[spec]),
        r#"{match,["b",[],"b"]}"#
    );
    let all = fx.atom("all");
    let spec = fx.capture3(all, "binary");
    renders!(
        fx,
        fx.run3("xabcy", "(z)?(b)", &[spec]),
        r#"{match,[<<"b">>,<<>>,<<"b">>]}"#
    );

    // `all_names` reports the named groups in name order (group 0 is never named);
    // a requested name that no group carries is `{-1,0}`.
    let all_names = fx.atom("all_names");
    let spec = fx.capture3(all_names, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(?<foo>b)(c)", &[spec]),
        "{match,[{2,1}]}"
    );
    let foo = fx.charlist("foo");
    let ids = fx.list(&[foo]);
    let spec = fx.capture3(ids, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(?<foo>b)(c)", &[spec]),
        "{match,[{2,1}]}"
    );
    let c = fx.charlist("c");
    let ids = fx.list(&[c]);
    let spec = fx.capture3(ids, "index");
    renders!(
        fx,
        fx.run3("xabcy", "(?<foo>b)(c)", &[spec]),
        "{match,[{-1,0}]}"
    );
}

#[test]
fn run_options_match_the_oracle_goldens() {
    let mut fx = Fixture::new();

    // `{offset, N}` skips the first `N` bytes before the automatic scan.
    let spec = fx.offset(2);
    renders!(fx, fx.run3("abcabc", "b", &[spec]), "{match,[{4,1}]}");

    // `caseless`, `dotall`, `multiline`, `extended` compile flags.
    let caseless = fx.atom("caseless");
    renders!(fx, fx.run3("ABC", "abc", &[caseless]), "{match,[{0,3}]}");
    renders!(fx, fx.run2("abc", "."), "{match,[{0,1}]}");
    let dotall = fx.atom("dotall");
    renders!(fx, fx.run3("a.c", ".", &[dotall]), "{match,[{0,1}]}");
    let multiline = fx.atom("multiline");
    renders!(fx, fx.run3("a\nb", "b$", &[multiline]), "{match,[{2,1}]}");
    renders!(fx, fx.run2("a\nb", "(?s)a.b"), "{match,[{0,3}]}");
    let extended = fx.atom("extended");
    renders!(
        fx,
        fx.run3("abc", "a  b  c", &[extended]),
        "{match,[{0,3}]}"
    );

    // `notempty` refuses the empty match; `notbol` drops the `^` anchor;
    // `anchored` fixes the start position; `report_errors` alone is a no-op.
    let notempty = fx.atom("notempty");
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(fx, fx.run3("", "", &[notempty, spec]), "nomatch");
    let notbol = fx.atom("notbol");
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(fx, fx.run3("abc", "b", &[notbol, spec]), "{match,[{1,1}]}");
    let anchored = fx.atom("anchored");
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(
        fx,
        fx.run3("abc", "a", &[anchored, spec]),
        "{match,[{0,1}]}"
    );
    let anchored = fx.atom("anchored");
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    renders!(fx, fx.run3("bac", "a", &[anchored, spec]), "nomatch");
    let report_errors = fx.atom("report_errors");
    renders!(fx, fx.run3("abc", "a", &[report_errors]), "{match,[{0,1}]}");

    // An exhausted `{match_limit, _}` is `{error,match_limit}` under
    // `report_errors` and the bare `nomatch` without it.
    let spec = fx.match_limit(0);
    let report_errors = fx.atom("report_errors");
    renders!(
        fx,
        fx.run3("abc", "a", &[spec, report_errors]),
        "{error,match_limit}"
    );
    let spec = fx.match_limit(0);
    renders!(fx, fx.run3("abc", "a", &[spec]), "nomatch");
}

#[test]
fn syntax_anchors_classes_and_quantifiers_match_the_oracle_goldens() {
    let mut fx = Fixture::new();

    // Anchors: `\A`/`\z`, `\b`, and the empty-match cases (`^`, `""`).
    renders!(fx, fx.run2("abc", r"\Aabc"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("abc", r"bc\z"), "{match,[{1,2}]}");
    renders!(fx, fx.run2("abc", r"\bb"), "nomatch");
    renders!(fx, fx.run2("abc", "^"), "{match,[{0,0}]}");
    renders!(fx, fx.run2("abc", ""), "{match,[{0,0}]}");
    renders!(fx, fx.run2("", ""), "{match,[{0,0}]}");

    // Character classes and the POSIX/escape shorthands.
    renders!(fx, fx.run2("abc", "[a-c]+"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("abc1", r"\d+"), "{match,[{3,1}]}");
    renders!(fx, fx.run2("abc_1", r"\w+"), "{match,[{0,5}]}");
    renders!(fx, fx.run2("a b", r"\s"), "{match,[{1,1}]}");
    renders!(fx, fx.run2("abc", "[[:alpha:]]+"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("a+b", r"\Qa+b\E"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("A", r"\x{41}"), "{match,[{0,1}]}");

    // Quantifiers: `{n,}`, `{n,m}`, lazy `+?`, possessive `*+`, and the
    // last-iteration capture of `(a)*`.
    renders!(fx, fx.run2("aaa", "a{2,}"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("aaaa", "a{2,3}"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("aaa", "a+?"), "{match,[{0,1}]}");
    renders!(fx, fx.run2("a", "(a)*+"), "{match,[{0,1},{0,1}]}");
    renders!(fx, fx.run2("aaa", "(a)*"), "{match,[{0,3},{2,1}]}");

    // Backreferences and inline option groups (whole-pattern and scoped).
    renders!(
        fx,
        fx.run2("abab", r"(a)(b)\1"),
        "{match,[{0,3},{0,1},{1,1}]}"
    );
    renders!(fx, fx.run2("ABC", "(?i)abc"), "{match,[{0,3}]}");
    renders!(fx, fx.run2("abc", "a(?i:BC)"), "{match,[{0,3}]}");
}

#[test]
fn compile_errors_match_the_oracle_goldens() {
    let mut fx = Fixture::new();

    // The three compile failures with their exact PCRE2 message and 0-based
    // byte position.
    renders!(
        fx,
        fx.compile1("a(b"),
        r#"{error,{"missing closing parenthesis",3}}"#
    );
    renders!(
        fx,
        fx.compile1("[a"),
        r#"{error,{"missing terminating ] for character class",2}}"#
    );
    renders!(
        fx,
        fx.compile1("a{2,1}"),
        r#"{error,{"numbers out of order in {} quantifier",5}}"#
    );
    // A duplicate subpattern name is refused unless `dupnames` is set. Its message
    // is long enough that `~p` re-flows the line at the oracle's break threshold,
    // so the exact bytes and 0-based position are asserted structurally rather
    // than through the re-flowing render (the shorter messages above also render
    // on one line, so they are pinned as goldens).
    let out = fx
        .compile1("(?<n>a)(?<n>b)")
        .expect("a compile error is a value");
    assert_eq!(fx.arity(out), 2, "an {{error, Reason}} 2-tuple");
    assert!(fx.is_atom(fx.elem(out, 0), "error"), "the `error` tag");
    let inner = fx.elem(out, 1);
    assert_eq!(fx.arity(inner), 2, "the {{Msg, Pos}} pair");
    assert_eq!(
        charlist_bytes(fx.elem(inner, 0)),
        b"two named subpatterns have the same name (PCRE2_DUPNAMES not set)".to_vec(),
        "the exact PCRE2 message"
    );
    assert_eq!(
        signed_value(fx.elem(inner, 1)),
        12,
        "the 0-based byte position"
    );

    // With `dupnames` the same pattern compiles: `{ok, {re_pattern, 2, 0, 0, Image}}`.
    let dupnames = fx.atom("dupnames");
    let out = fx
        .compile2("(?<n>a)(?<n>b)", &[dupnames])
        .expect("dupnames compiles");
    assert!(fx.is_atom(fx.elem(out, 0), "ok"), "the `ok` tag");
    let mp = fx.elem(out, 1);
    assert!(fx.is_atom(fx.elem(mp, 0), "re_pattern"), "the pattern tag");
    assert_eq!(fx.arity(mp), 5, "the compiled-pattern tuple shape");
    assert_eq!(signed_value(fx.elem(mp, 1)), 2, "NCap = 2 capture groups");
    assert_eq!(signed_value(fx.elem(mp, 2)), 0, "no unicode flag");
    assert_eq!(signed_value(fx.elem(mp, 3)), 0, "newline lf");
}

#[test]
fn inspect_version_and_export_import_match_the_oracle_goldens() {
    let mut fx = Fixture::new();

    // `re:inspect(MP, namelist)` — the unique subpattern names as binaries.
    let out = fx.compile1("(?<A>A)|(?<B>B)|(?<C>C)").expect("compiles");
    let mp = fx.elem(out, 1);
    renders!(
        fx,
        fx.inspect_namelist(mp),
        r#"{namelist,[<<"A">>,<<"B">>,<<"C">>]}"#
    );

    // `re:version/0`.
    renders!(fx, fx.ok("version", &[]), r#"<<"10.47 2025-10-21">>"#);

    // `compile (export)` → `import` → `run`: the imported `mp()` matches exactly
    // as the directly-compiled one does.
    let export = fx.atom("export");
    let out = fx
        .compile2("(b)(c)", &[export])
        .expect("compiles with export");
    assert!(fx.is_atom(fx.elem(out, 0), "ok"), "the `ok` tag");
    let exported = fx.elem(out, 1);
    assert!(
        fx.is_atom(fx.elem(exported, 0), "re_exported_pattern"),
        "the export tag"
    );
    assert_eq!(fx.arity(exported), 5, "the exported-pattern tuple shape");

    let imported = fx.ok("import", &[exported]).expect("imports");
    assert!(
        fx.is_atom(fx.elem(imported, 0), "re_pattern"),
        "the rebuilt tag"
    );
    assert_eq!(fx.arity(imported), 5);
    renders!(
        fx,
        fx.run_with("xabcy", imported, &[]),
        "{match,[{2,2},{2,1},{3,1}]}"
    );
    // ... and identically to the directly-compiled pattern.
    renders!(
        fx,
        fx.run2("xabcy", "(b)(c)"),
        "{match,[{2,2},{2,1},{3,1}]}"
    );
}

#[test]
fn internal_run_matches_the_oracle_goldens() {
    let mut fx = Fixture::new();

    let out = fx.compile1("(b)(c)").expect("compiles");
    let mp = fx.elem(out, 1);

    // `internal_run/4` is `run/3` plus the internal "first call" flag; the same
    // default `all`/`index` capture shape, flat for a non-global run.
    let offset = fx.offset(0);
    renders!(
        fx,
        fx.internal_run("xabcy", mp, &[offset], true),
        "{match,[{2,2},{2,1},{3,1}]}"
    );
    // With `global` it nests one list per match, exactly as `run/3` does.
    let global = fx.atom("global");
    let offset = fx.offset(0);
    renders!(
        fx,
        fx.internal_run("xabcy", mp, &[global, offset], true),
        "{match,[[{2,2},{2,1},{3,1}]]}"
    );
    // A non-matching subject is `nomatch`.
    let out = fx.compile1("(b)(c)").expect("compiles");
    let mp = fx.elem(out, 1);
    let offset = fx.offset(0);
    renders!(fx, fx.internal_run("zzz", mp, &[offset], true), "nomatch");
}

#[test]
fn non_iodata_arguments_are_badarg() {
    let mut fx = Fixture::new();

    // A non-iodata RE / pattern is `badarg`, never a panic.
    let subject = fx.charlist("abc");
    let int = make_small(123);
    assert_badarg(
        fx.call("re", "run", &[subject, int]),
        "re:run/2 with a non-iodata RE",
    );
    let int = make_small(123);
    assert_badarg(
        fx.call("re", "compile", &[int]),
        "re:compile/1 with a non-iodata pattern",
    );
}

// ---------------------------------------------------------------------------
// (b) Live-oracle cross-check of the core cases
// ---------------------------------------------------------------------------

#[test]
fn core_cases_cross_check_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs re: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("========================================================");
        eprintln!();
        return;
    };

    // The core representative cases (the exhaustive oracle sweep lives in the
    // crate's internal suite): capture shapes, `global`, `offset`, the three
    // compile-error positions, `inspect(namelist)` and `version/0`. Each entry is
    // the oracle expression plus this crate's observable term for the same call.
    // Bare BIF names are not auto-imported under `-eval`, so the expressions fully
    // qualify every call.
    let mut fx = Fixture::new();
    let mut cases: Vec<(&str, Eterm)> = Vec::new();
    cases.push((
        r#"re:run("xabcy","b")"#,
        fx.run2("xabcy", "b").expect("matches"),
    ));
    cases.push((
        r#"re:run("xabcy","(b)(c)")"#,
        fx.run2("xabcy", "(b)(c)").expect("matches"),
    ));
    let global = fx.atom("global");
    cases.push((
        r#"re:run("abab","a",[global])"#,
        fx.run3("abab", "a", &[global]).expect("matches"),
    ));
    let global = fx.atom("global");
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    cases.push((
        r#"re:run("abab","(a)(b)",[global,{capture,all,index}])"#,
        fx.run3("abab", "(a)(b)", &[global, spec]).expect("matches"),
    ));
    let all = fx.atom("all");
    let spec = fx.capture3(all, "list");
    cases.push((
        r#"re:run("xabcy","(b)(c)",[{capture,all,list}])"#,
        fx.run3("xabcy", "(b)(c)", &[spec]).expect("matches"),
    ));
    let all = fx.atom("all");
    let spec = fx.capture3(all, "binary");
    cases.push((
        r#"re:run("xabcy","(b)(c)",[{capture,all,binary}])"#,
        fx.run3("xabcy", "(b)(c)", &[spec]).expect("matches"),
    ));
    let all = fx.atom("all");
    let spec = fx.capture3(all, "index");
    cases.push((
        r#"re:run("xabcy","(z)?(b)",[{capture,all,index}])"#,
        fx.run3("xabcy", "(z)?(b)", &[spec]).expect("matches"),
    ));
    let none = fx.atom("none");
    let spec = fx.capture2(none);
    cases.push((
        r#"re:run("xabcy","b",[{capture,none}])"#,
        fx.run3("xabcy", "b", &[spec]).expect("matches"),
    ));
    let spec = fx.offset(2);
    cases.push((
        r#"re:run("abcabc","b",[{offset,2}])"#,
        fx.run3("abcabc", "b", &[spec]).expect("matches"),
    ));
    cases.push((
        r#"re:compile("a(b")"#,
        fx.compile1("a(b").expect("a compile error is a value"),
    ));
    cases.push((
        r#"re:compile("[a")"#,
        fx.compile1("[a").expect("a compile error is a value"),
    ));
    cases.push((
        r#"re:compile("a{2,1}")"#,
        fx.compile1("a{2,1}").expect("a compile error is a value"),
    ));
    cases.push((
        r#"begin {ok,MP}=re:compile("(?<A>A)|(?<B>B)|(?<C>C)"), re:inspect(MP,namelist) end"#,
        {
            let out = fx.compile1("(?<A>A)|(?<B>B)|(?<C>C)").expect("compiles");
            let mp = fx.elem(out, 1);
            fx.inspect_namelist(mp).expect("inspect fits")
        },
    ));
    cases.push((
        r#"re:version()"#,
        fx.ok("version", &[]).expect("version fits"),
    ));

    // One spawn: print each expression's `~p` image, one per line.
    let mut eval = String::new();
    for (expr, _) in &cases {
        eval.push_str(&format!("io:format(\"~p~n\",[{expr}]),"));
    }
    eval.push_str("halt(0).");
    let stdout = run_erl(&oracle, &eval);

    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    for (expr, term) in &cases {
        let expected = lines
            .next()
            .unwrap_or_else(|| panic!("the oracle printed fewer lines than the corpus:\n{stdout}"));
        let got = fx.show(*term);
        assert_eq!(got, expected, "the oracle disagrees for `{expr}`");
    }
    assert!(
        lines.next().is_none(),
        "the oracle printed more lines than the corpus:\n{stdout}"
    );
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs re] $ERTS_ORACLE='{}' is not an existing executable \
             — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Runs one `erl -noshell -eval <eval>` (whose body ends in `halt(0).`) and
/// returns its stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}
