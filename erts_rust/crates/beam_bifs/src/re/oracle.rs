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

//! The **live-oracle differential harness** for the in-crate `re` engine — the
//! independent check that does not trust any golden written down by hand.
//!
//! Every other test in this node (`mod.rs`, `run.rs`, `render.rs`, …) pins a
//! *golden* the task author transcribed from the oracle by hand, so a
//! transcription slip would be frozen into the suite. This harness instead runs
//! each case twice — once under the real system `erl` and once through this
//! crate's real dispatch path (`crate::registry::default_registry()` →
//! `re:run/2,3`) — and requires the two to agree.
//!
//! **What is compared:** the *ETF bytes* of the result. Both sides encode their
//! result term with `erlang:term_to_binary/1` (the oracle's own, and this crate's
//! registered BIF) and the comparison is over the uppercase/lowercase hex of
//! those bytes, so it is **printer-free and byte-exact**: it pins the term
//! *structure* — atom names, tuple/list/cons layout, integers, binary payloads —
//! with no dependency on any `~p`/`~w` rendering rule.
//!
//! **What is deliberately NOT compared:**
//!
//! * the opaque `Payload` in element 5 of a compiled-pattern term — this node's
//!   compiled image is *its own*, never PCRE's, so a `re:compile` result could
//!   never compare byte-for-byte (this corpus therefore stays on the run path,
//!   which returns only plain result terms);
//! * the exception *reason* on the run path — the harness records only **that**
//!   both sides raised (`re:run("abc",123)` is the sole raising case here), so a
//!   `badarg` and a `function_clause` would be indistinguishable. Pinning the
//!   reason needs a `catch Class:Reason` comparison, which is a different (and
//!   later) check.
//!
//! **Skip-loudly policy:** without a usable `erl` (no `$ERTS_ORACLE` naming an
//! executable and none on `$PATH`), or if the oracle cannot answer a case, the
//! test prints a `SKIP:` line and returns **without failing** — the in-suite
//! golden assertions remain the no-oracle evidence.
//!
//! # Why this file is long
//!
//! This is a **test data file**, not logic: each corpus is a flat table of case
//! literals grown one area at a time (literals, classes, anchors, quantifiers,
//! captures, options, Unicode, error inputs), each row a pattern plus its subject
//! and options, all running through the one shared sweep driver. Splitting the
//! corpus across files would scatter one table and make a duplicate or missing
//! case harder to see, so it stays in one place.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, AtomTable, Eterm, NIL};

use crate::bits;
use crate::context::BifContext;
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// Fixture: a registry + heap + seeded atom table, driven the way `beam_rt` does
// ---------------------------------------------------------------------------

/// A registry + heap + seeded atom table the cases build their arguments on and
/// run the `re` BIFs through. No optional seam is installed — `re:run/2,3` needs
/// none, and neither does `erlang:term_to_binary/1` for these plain result terms.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed([
            // Result vocabulary.
            "match",
            "nomatch",
            "match_limit",
            "error",
            "compile",
            "ok",
            "true",
            "false",
            "re_pattern",
            "re_exported_pattern",
            // Run options (`options::parse` resolves names through this table).
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
            "report_errors",
        ]);
        Fixture {
            registry: crate::registry::default_registry(),
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms,
        }
    }

    /// A seeded atom term.
    fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("a seeded atom"))
    }

    /// The on-heap binary `<<bytes>>`.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        bits::make_binary(&mut self.heap, bytes).expect("the binary fits the heap")
    }

    /// A charlist of `s`'s codepoints.
    fn charlist(&mut self, s: &str) -> Eterm {
        let elems: Vec<Eterm> =
            s.chars().map(|c| make_small(i64::from(c as u32))).collect();
        self.list(&elems)
    }

    /// A proper list of `elems`.
    fn list(&mut self, elems: &[Eterm]) -> Eterm {
        let mut tail = NIL;
        for &elem in elems.iter().rev() {
            tail = self.heap.cons(elem, tail).expect("the list fits the heap");
        }
        tail
    }

    /// A tuple of `elems`.
    fn tuple(&mut self, elems: &[Eterm]) -> Eterm {
        self.heap.tuple(elems).expect("the tuple fits the heap")
    }

    /// Calls `module:function(Args)` through the real registry.
    fn call_fn(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry.call(module, function, arity, &mut ctx, args)
    }

    /// `re:run/2` (empty option list) or `re:run/3` on already-built terms.
    fn run_opts(&mut self, subject: Eterm, re: Eterm, opts: &[Eterm]) -> Result<Eterm> {
        if opts.is_empty() {
            self.call_fn("re", "run", 2, &[subject, re])
        } else {
            let list = self.list(opts);
            self.call_fn("re", "run", 3, &[subject, re, list])
        }
    }

    /// The lowercase hex of `term`'s `erlang:term_to_binary/1` image, encoded with
    /// this crate's **registered** BIF (so the comparison exercises the same
    /// encoder the oracle does).
    fn encode_hex(&mut self, term: Eterm) -> Result<String> {
        let image = self.call_fn("erlang", "term_to_binary", 1, &[term])?;
        let bytes = bits::bytes(image)?;
        Ok(hex_lower(&bytes))
    }
}

// ---------------------------------------------------------------------------
// Tiny builders, so each case's Rust side mirrors its Erlang expression
// ---------------------------------------------------------------------------

/// An atom.
fn a(f: &Fixture, name: &str) -> Eterm {
    f.atom(name)
}

/// A charlist (`"…"` in Erlang).
fn chr(f: &mut Fixture, s: &str) -> Eterm {
    f.charlist(s)
}

/// A binary (`<<"…">>` in Erlang).
fn bin(f: &mut Fixture, bytes: &[u8]) -> Eterm {
    f.binary(bytes)
}

/// A list.
fn l(f: &mut Fixture, elems: &[Eterm]) -> Eterm {
    f.list(elems)
}

/// A tuple.
fn t(f: &mut Fixture, elems: &[Eterm]) -> Eterm {
    f.tuple(elems)
}

/// The `{capture, Value}` option.
fn cap(f: &mut Fixture, value: Eterm) -> Eterm {
    let tag = a(f, "capture");
    t(f, &[tag, value])
}

/// The `{capture, Value, Type}` option.
fn cap_ty(f: &mut Fixture, value: Eterm, ty: &str) -> Eterm {
    let tag = a(f, "capture");
    let ty = a(f, ty);
    t(f, &[tag, value, ty])
}

/// A plain `{Name, Value}` option (`{offset, N}`, …).
fn pair(f: &mut Fixture, name: &str, value: Eterm) -> Eterm {
    let tag = a(f, name);
    t(f, &[tag, value])
}

/// The lowercase hex of `bytes` (`binary:encode_hex/1` lowercased).
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0F)]));
    }
    out
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// One differential case: the Erlang expression (authoritative) and the Rust-side
/// call that must reproduce it on the same subject/pattern/options.
struct Case {
    /// A short label used in the mismatch report.
    name: &'static str,
    /// The `re:…` expression, written exactly as it must appear inside `-eval`.
    erl: &'static str,
    /// The same call over the fixture.
    run: fn(&mut Fixture) -> Result<Eterm>,
}

/// The first corpus: capture shapes, `global` and `{offset, N}` (31 cases).
///
/// Every `erl` string is the *real* Erlang expression; each `run` closure builds
/// the arguments term-for-term so the two sides visibly mirror one another.
fn corpus() -> Vec<Case> {
    vec![
        // ---- capture shapes: run/2,3 ------------------------------------
        Case {
            name: "run2/simple",
            erl: r#"re:run("xabcy","b")"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"b");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "run2/two-groups",
            erl: r#"re:run("xabcy","(b)(c)")"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "capture/none",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,none}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let none = a(f, "none");
                let spec = cap(f, none);
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/empty-list",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,[]}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let empty = l(f, &[]);
                let spec = cap(f, empty);
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/all-index",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/all-list",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,all,list}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "list");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/all-binary",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,all,binary}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "binary");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/all_but_first-index",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,all_but_first,index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let value = a(f, "all_but_first");
                let spec = cap_ty(f, value, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/first-index",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,first,index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let first = a(f, "first");
                let spec = cap_ty(f, first, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/first-list",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,first,list}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let first = a(f, "first");
                let spec = cap_ty(f, first, "list");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/nonparticipating-index",
            erl: r#"re:run("xabcy","(z)?(b)",[{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(z)?(b)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/nonparticipating-list",
            erl: r#"re:run("xabcy","(z)?(b)",[{capture,all,list}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(z)?(b)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "list");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/nonparticipating-binary",
            erl: r#"re:run("xabcy","(z)?(b)",[{capture,all,binary}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(z)?(b)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "binary");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/all_names-index",
            erl: r#"re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(?<foo>b)(c)");
                let value = a(f, "all_names");
                let spec = cap_ty(f, value, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/all_names-list",
            erl: r#"re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,list}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(?<foo>b)(c)");
                let value = a(f, "all_names");
                let spec = cap_ty(f, value, "list");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/ids-index",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,[2,1],index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let ids = l(f, &[make_small(2), make_small(1)]);
                let spec = cap_ty(f, ids, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/ids-list",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,[2,1],list}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let ids = l(f, &[make_small(2), make_small(1)]);
                let spec = cap_ty(f, ids, "list");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/ids-binary",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,[2,1],binary}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let ids = l(f, &[make_small(2), make_small(1)]);
                let spec = cap_ty(f, ids, "binary");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/out-of-range-id",
            erl: r#"re:run("xabcy","(b)(c)",[{capture,[5],index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let ids = l(f, &[make_small(5)]);
                let spec = cap_ty(f, ids, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "capture/names-mixed",
            erl: r#"re:run("xabcy","(?<foo>b)(c)",[{capture,["foo","zz"],list}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(?<foo>b)(c)");
                let foo = chr(f, "foo");
                let zz = chr(f, "zz");
                let names = l(f, &[foo, zz]);
                let spec = cap_ty(f, names, "list");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "nomatch",
            erl: r#"re:run("xabcy","z")"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"z");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "badarg/non-iodata-re",
            erl: r#"re:run("abc",123)"#,
            run: |f| {
                let subject = chr(f, "abc");
                let re = make_small(123);
                f.run_opts(subject, re, &[])
            },
        },
        // ---- global -----------------------------------------------------
        Case {
            name: "global/bare",
            erl: r#"re:run("abab","a",[global])"#,
            run: |f| {
                let subject = chr(f, "abab");
                let pattern = bin(f, b"a");
                let global = a(f, "global");
                f.run_opts(subject, pattern, &[global])
            },
        },
        Case {
            name: "global/all-index",
            erl: r#"re:run("abab","(a)(b)",[global,{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "abab");
                let pattern = bin(f, b"(a)(b)");
                let global = a(f, "global");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[global, spec])
            },
        },
        Case {
            name: "global/none",
            erl: r#"re:run("abab","a",[global,{capture,none}])"#,
            run: |f| {
                let subject = chr(f, "abab");
                let pattern = bin(f, b"a");
                let global = a(f, "global");
                let none = a(f, "none");
                let spec = cap(f, none);
                f.run_opts(subject, pattern, &[global, spec])
            },
        },
        Case {
            name: "global/offset",
            erl: r#"re:run("abcabc","b",[global,{offset,2}])"#,
            run: |f| {
                let subject = chr(f, "abcabc");
                let pattern = bin(f, b"b");
                let global = a(f, "global");
                let spec = pair(f, "offset", make_small(2));
                f.run_opts(subject, pattern, &[global, spec])
            },
        },
        Case {
            name: "global/empty-match",
            erl: r#"re:run("abc","x{0}",[global])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"x{0}");
                let global = a(f, "global");
                f.run_opts(subject, pattern, &[global])
            },
        },
        // ---- {offset, N} ------------------------------------------------
        Case {
            name: "offset/in-range",
            erl: r#"re:run("abcabc","b",[{offset,2}])"#,
            run: |f| {
                let subject = chr(f, "abcabc");
                let pattern = bin(f, b"b");
                let spec = pair(f, "offset", make_small(2));
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "offset/at-end",
            erl: r#"re:run("abc","b",[{offset,3}])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"b");
                let spec = pair(f, "offset", make_small(3));
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "offset/past-end",
            erl: r#"re:run("abc","b",[{offset,5}])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"b");
                let spec = pair(f, "offset", make_small(5));
                f.run_opts(subject, pattern, &[spec])
            },
        },
        // ---- a binary subject -------------------------------------------
        Case {
            name: "binary-subject",
            erl: r#"re:run(<<"xabcy">>,"b")"#,
            run: |f| {
                let subject = bin(f, b"xabcy");
                let pattern = bin(f, b"b");
                f.run_opts(subject, pattern, &[])
            },
        },
    ]
}

/// The option vocabulary [`option_flag_corpus`]'s closures build their argument
/// terms from.
///
/// These are exactly the option *names* a case above mentions but the shared
/// [`Fixture::new`] seed does not carry. Every one must be interned on the
/// fixture before the sweep, for two reasons: `Fixture::atom`/`a` resolve a name
/// through that table, and `options::parse` names each flag atom through it too.
const OPTION_FLAG_ATOMS: &[&str] = &[
    "caseless",
    "dotall",
    "multiline",
    "dollar_endonly",
    "extended",
    "ungreedy",
    "firstline",
    "no_auto_capture",
    "newline",
    "crlf",
    "anchored",
    "notempty",
    "notempty_atstart",
    "notbol",
    "noteol",
    "match_limit",
];

/// The second corpus: the run/compile **option flags** (25 cases).
///
/// Each case is a `re:run` whose option list switches on one (or, for the
/// combined cases, two) of `parse_options`' flags, so the byte-for-byte result
/// pins the flag's *effect* — not merely that the option is accepted. The flags
/// covered, with their cases:
///
/// * `caseless` (1–4) — a folded literal, a folded backreference, and the
///   `nomatch` when the fold cannot bridge the pattern;
/// * `dotall` (5–6) — `.` crossing a newline, and refusing to;
/// * `multiline` (7–8) — `$`/`^` on an interior line boundary, incl. `global`;
/// * `dollar_endonly` (9) — `$` restricted to the very end;
/// * `extended` (10–11) — free whitespace outside a class, literal inside one;
/// * `ungreedy` (12–13) — the default inversion, and the explicit-`?` flip-back;
/// * `firstline` (14) — the match must start on the first line;
/// * `no_auto_capture` (15) — unnamed groups stop capturing;
/// * `{newline, crlf}` (16) — `^` after a CRLF pair;
/// * `anchored` (17–18) — the start position is not searched;
/// * `notempty` (19) / `notempty_atstart` (20) — the two empty-match refusals;
/// * `notbol` (21) / `noteol` (22) — the two end-anchor suppressions;
/// * `report_errors` (23–24) with `{match_limit, 0}` (24–25) — the limit's
///   exhaustion is the returned `{error, match_limit}` value, or `nomatch`.
fn option_flag_corpus() -> Vec<Case> {
    vec![
        // ---- caseless ---------------------------------------------------
        Case {
            name: "caseless/simple",
            erl: r#"re:run("ABC","abc",[caseless])"#,
            run: |f| {
                let subject = chr(f, "ABC");
                let pattern = bin(f, b"abc");
                let caseless = a(f, "caseless");
                f.run_opts(subject, pattern, &[caseless])
            },
        },
        Case {
            name: "caseless/all-index",
            erl: r#"re:run("xabcy","(b)(c)",[caseless,{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let caseless = a(f, "caseless");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[caseless, spec])
            },
        },
        Case {
            name: "caseless/backref",
            erl: r#"re:run("aA","(a)\\1",[caseless])"#,
            run: |f| {
                let subject = chr(f, "aA");
                let pattern = bin(f, b"(a)\\1");
                let caseless = a(f, "caseless");
                f.run_opts(subject, pattern, &[caseless])
            },
        },
        Case {
            name: "caseless/nomatch",
            erl: r#"re:run("ABC","abd",[caseless])"#,
            run: |f| {
                let subject = chr(f, "ABC");
                let pattern = bin(f, b"abd");
                let caseless = a(f, "caseless");
                f.run_opts(subject, pattern, &[caseless])
            },
        },
        // ---- dotall -----------------------------------------------------
        Case {
            name: "dotall/match",
            erl: r#"re:run("a\nb","a.b",[dotall])"#,
            run: |f| {
                let subject = chr(f, "a\nb");
                let pattern = bin(f, b"a.b");
                let dotall = a(f, "dotall");
                f.run_opts(subject, pattern, &[dotall])
            },
        },
        Case {
            name: "dotall/nomatch-without",
            erl: r#"re:run("a\nb","a.b")"#,
            run: |f| {
                let subject = chr(f, "a\nb");
                let pattern = bin(f, b"a.b");
                f.run_opts(subject, pattern, &[])
            },
        },
        // ---- multiline --------------------------------------------------
        Case {
            name: "multiline/end-anchor",
            erl: r#"re:run("a\nb","b$",[multiline])"#,
            run: |f| {
                let subject = chr(f, "a\nb");
                let pattern = bin(f, b"b$");
                let multiline = a(f, "multiline");
                f.run_opts(subject, pattern, &[multiline])
            },
        },
        Case {
            name: "multiline/global-caret",
            erl: r#"re:run("a\nb\n","^",[global,multiline])"#,
            run: |f| {
                let subject = chr(f, "a\nb\n");
                let pattern = bin(f, b"^");
                let global = a(f, "global");
                let multiline = a(f, "multiline");
                f.run_opts(subject, pattern, &[global, multiline])
            },
        },
        // ---- dollar_endonly ---------------------------------------------
        Case {
            name: "dollar_endonly/nomatch",
            erl: r#"re:run("a\nb","a$",[dollar_endonly])"#,
            run: |f| {
                let subject = chr(f, "a\nb");
                let pattern = bin(f, b"a$");
                let dollar_endonly = a(f, "dollar_endonly");
                f.run_opts(subject, pattern, &[dollar_endonly])
            },
        },
        // ---- extended ---------------------------------------------------
        Case {
            name: "extended/whitespace",
            erl: r#"re:run("abc","a  b  c",[extended])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a  b  c");
                let extended = a(f, "extended");
                f.run_opts(subject, pattern, &[extended])
            },
        },
        Case {
            name: "extended/class-space",
            erl: r#"re:run("abc","a[ ]b[ ]c",[extended])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a[ ]b[ ]c");
                let extended = a(f, "extended");
                f.run_opts(subject, pattern, &[extended])
            },
        },
        // ---- ungreedy ---------------------------------------------------
        Case {
            name: "ungreedy/lazy-default",
            erl: r#"re:run("aaa","a+",[ungreedy])"#,
            run: |f| {
                let subject = chr(f, "aaa");
                let pattern = bin(f, b"a+");
                let ungreedy = a(f, "ungreedy");
                f.run_opts(subject, pattern, &[ungreedy])
            },
        },
        Case {
            name: "ungreedy/flips-explicit-lazy",
            erl: r#"re:run("aaa","a+?",[ungreedy])"#,
            run: |f| {
                let subject = chr(f, "aaa");
                let pattern = bin(f, b"a+?");
                let ungreedy = a(f, "ungreedy");
                f.run_opts(subject, pattern, &[ungreedy])
            },
        },
        // ---- firstline --------------------------------------------------
        Case {
            name: "firstline/nomatch",
            erl: r#"re:run("x\naaa","a+",[firstline])"#,
            run: |f| {
                let subject = chr(f, "x\naaa");
                let pattern = bin(f, b"a+");
                let firstline = a(f, "firstline");
                f.run_opts(subject, pattern, &[firstline])
            },
        },
        // ---- no_auto_capture --------------------------------------------
        Case {
            name: "no_auto_capture/suppresses-groups",
            erl: r#"re:run("xabcy","(b)(c)",[no_auto_capture])"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let no_auto_capture = a(f, "no_auto_capture");
                f.run_opts(subject, pattern, &[no_auto_capture])
            },
        },
        // ---- {newline, crlf} --------------------------------------------
        Case {
            name: "newline_crlf/multiline-caret",
            erl: r#"re:run("a\r\nb","^b",[multiline,{newline,crlf}])"#,
            run: |f| {
                let subject = chr(f, "a\r\nb");
                let pattern = bin(f, b"^b");
                let multiline = a(f, "multiline");
                let crlf = a(f, "crlf");
                let newline = pair(f, "newline", crlf);
                f.run_opts(subject, pattern, &[multiline, newline])
            },
        },
        // ---- anchored ---------------------------------------------------
        Case {
            name: "anchored/nomatch",
            erl: r#"re:run("bac","a",[anchored,{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "bac");
                let pattern = bin(f, b"a");
                let anchored = a(f, "anchored");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[anchored, spec])
            },
        },
        Case {
            name: "anchored/match",
            erl: r#"re:run("abc","a",[anchored,{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a");
                let anchored = a(f, "anchored");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[anchored, spec])
            },
        },
        // ---- notempty / notempty_atstart --------------------------------
        Case {
            name: "notempty/refuses-empty",
            erl: r#"re:run("","",[notempty,{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "");
                let pattern = bin(f, b"");
                let notempty = a(f, "notempty");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[notempty, spec])
            },
        },
        Case {
            name: "notempty_atstart/match-later",
            erl: r#"re:run("abc","b*",[notempty_atstart,{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"b*");
                let notempty_atstart = a(f, "notempty_atstart");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[notempty_atstart, spec])
            },
        },
        // ---- notbol / noteol --------------------------------------------
        Case {
            name: "notbol/refuses-start",
            erl: r#"re:run("abc","^a",[notbol])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"^a");
                let notbol = a(f, "notbol");
                f.run_opts(subject, pattern, &[notbol])
            },
        },
        Case {
            name: "noteol/refuses-end",
            erl: r#"re:run("abc","c$",[noteol])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"c$");
                let noteol = a(f, "noteol");
                f.run_opts(subject, pattern, &[noteol])
            },
        },
        // ---- report_errors / {match_limit, 0} ---------------------------
        Case {
            name: "report_errors/match",
            erl: r#"re:run("abc","a",[report_errors])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a");
                let report_errors = a(f, "report_errors");
                f.run_opts(subject, pattern, &[report_errors])
            },
        },
        Case {
            name: "match_limit_zero/report-errors",
            erl: r#"re:run("abc","a",[{match_limit,0},report_errors])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a");
                let spec = pair(f, "match_limit", make_small(0));
                let report_errors = a(f, "report_errors");
                f.run_opts(subject, pattern, &[spec, report_errors])
            },
        },
        Case {
            name: "match_limit_zero/nomatch",
            erl: r#"re:run("abc","a",[{match_limit,0}])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a");
                let spec = pair(f, "match_limit", make_small(0));
                f.run_opts(subject, pattern, &[spec])
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// The oracle side: one `erl -noshell -eval …` per case, bounded and skippable
// ---------------------------------------------------------------------------

/// One side's answer: a `term_to_binary` image (lowercase hex), or "raised".
#[derive(Debug)]
enum Answer {
    Raises,
    Hex(String),
}

/// Wall-clock budget for a single `erl` probe before the oracle is declared
/// unusable (a wedged `erl` is killed, never allowed to hang the suite).
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The `-eval` program for one case.
///
/// It prints the lowercase-safe hex of the result's `term_to_binary` image, or
/// the literal `raise` when the expression raises. The trailing `halt(0)` is
/// **required**: `erl -noshell -eval` does not stop on its own once the
/// expression is evaluated, so without it the node would linger to the timeout.
fn erl_statement(expr: &str) -> String {
    format!(
        "io:format(\"~s~n\",[try binary:encode_hex(term_to_binary({expr})) \
         catch _:_ -> <<\"raise\">> end]), halt(0)."
    )
}

/// Runs one case under the oracle, or `None` when the oracle cannot answer (a
/// loud `SKIP:` has already been printed by then).
fn oracle_answer(oracle: &Path, expr: &str) -> Option<Answer> {
    let program = erl_statement(expr);
    let stdout = erl_stdout(oracle, &program)?;
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line == "raise" {
        Some(Answer::Raises)
    } else {
        // `binary:encode_hex/1` prints uppercase by default; the Rust side is
        // lowercase, so normalise the oracle's answer.
        Some(Answer::Hex(line.to_ascii_lowercase()))
    }
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs re] $ERTS_ORACLE='{}' is not an existing executable — \
             treating it as no oracle.",
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

/// Runs one `erl -noshell -eval <program>` under a bounded wall-clock budget and
/// returns its stdout, or `None` (after a loud skip message) when no oracle is
/// available / it cannot answer.
///
/// The child's stdout and stderr are drained on worker threads (so a full pipe can
/// never wedge it) while the main thread waits with a deadline; a child that
/// overruns [`ERL_TIMEOUT`] is **killed** and reaped, so a hung `erl` can never
/// hang the suite.
fn erl_stdout(oracle: &Path, program: &str) -> Option<String> {
    let spawned = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "[beam_bifs re] SKIP: could not spawn the oracle '{}': {e}",
                oracle.display()
            );
            return None;
        }
    };

    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + ERL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    eprintln!(
                        "[beam_bifs re] SKIP: the oracle did not answer within \
                         {ERL_TIMEOUT:?} and was killed."
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                eprintln!("[beam_bifs re] SKIP: waiting for the oracle failed: {e}");
                return None;
            }
        }
    };

    let stdout = join_pipe(stdout_reader, "stdout")?;
    let stderr = join_pipe(stderr_reader, "stderr")?;

    if !status.success() {
        eprintln!(
            "[beam_bifs re] SKIP: the oracle exited with {:?} for:\n{program}\n\
             --- stderr ---\n{}",
            status.code(),
            String::from_utf8_lossy(&stderr),
        );
        return None;
    }
    Some(String::from_utf8_lossy(&stdout).into_owned())
}

/// Joins a pipe-reader thread, turning a panic or an I/O error into a loud skip.
fn join_pipe(reader: thread::JoinHandle<std::io::Result<Vec<u8>>>, which: &str) -> Option<Vec<u8>> {
    match reader.join() {
        Ok(Ok(buf)) => Some(buf),
        Ok(Err(e)) => {
            eprintln!("[beam_bifs re] SKIP: reading the oracle {which} failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("[beam_bifs re] SKIP: the oracle {which} reader panicked");
            None
        }
    }
}

/// Reads a child pipe to EOF, surfacing an I/O error instead of swallowing it.
fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Runs the whole corpus twice — once through the real `erl`, once through this
/// crate's real dispatch path — and asserts the `term_to_binary` bytes agree
/// (or that both sides raised).
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
#[test]
fn the_live_oracle_agrees_on_the_re_corpus() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!("[beam_bifs re] oracle cross-check SKIPPED: no usable `erl` on $PATH");
        eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        return;
    };

    let cases = corpus();
    let mut fx = Fixture::new();
    let started = Instant::now();
    let mut ran = 0usize;

    for case in &cases {
        // Our side, over the real dispatch path.
        let ours = match (case.run)(&mut fx) {
            Ok(term) => match fx.encode_hex(term) {
                Ok(hex) => Answer::Hex(hex),
                Err(e) => panic!(
                    "case `{}` produced a term this crate cannot encode: {e:?}\n erl: {}",
                    case.name, case.erl
                ),
            },
            Err(_) => Answer::Raises,
        };

        // The oracle's answer for the same Erlang expression.
        let Some(theirs) = oracle_answer(&oracle, case.erl) else {
            eprintln!(
                "[beam_bifs re] oracle cross-check SKIPPED (the oracle could not answer \
                 case `{}`).",
                case.name
            );
            return;
        };

        match (&ours, &theirs) {
            (Answer::Raises, Answer::Raises) => {}
            (Answer::Hex(ours), Answer::Hex(theirs)) if ours == theirs => {}
            _ => panic!(
                "case `{}` disagrees with the oracle\n erl:    {}\n oracle: {theirs:?}\n ours:   {ours:?}",
                case.name, case.erl
            ),
        }
        ran += 1;
    }

    eprintln!(
        "[beam_bifs re] oracle cross-check RAN against '{}': {} cases agreed in {:?}.",
        oracle.display(),
        ran,
        started.elapsed(),
    );
}

/// The same differential sweep as [`the_live_oracle_agrees_on_the_re_corpus`],
/// over [`option_flag_corpus`] — the run/compile **option flags**.
///
/// The fixture is the shared [`Fixture::new`] one plus [`OPTION_FLAG_ATOMS`], so
/// each case's option atoms resolve both as term *constructors* (`a`) and as the
/// names `options::parse` reads back off the list.
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
#[test]
fn oracle_cross_check_option_flags() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!(
            "[beam_bifs re] option-flag oracle cross-check SKIPPED: no usable `erl` on $PATH"
        );
        eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        return;
    };

    let cases = option_flag_corpus();
    let mut fx = Fixture::new();
    // The option names the shared seed lacks (see `OPTION_FLAG_ATOMS`).
    fx.atoms.seed(OPTION_FLAG_ATOMS.iter().copied());
    let started = Instant::now();
    let mut ran = 0usize;

    for case in &cases {
        // Our side, over the real dispatch path.
        let ours = match (case.run)(&mut fx) {
            Ok(term) => match fx.encode_hex(term) {
                Ok(hex) => Answer::Hex(hex),
                Err(e) => panic!(
                    "case `{}` produced a term this crate cannot encode: {e:?}\n erl: {}",
                    case.name, case.erl
                ),
            },
            Err(_) => Answer::Raises,
        };

        // The oracle's answer for the same Erlang expression.
        let Some(theirs) = oracle_answer(&oracle, case.erl) else {
            eprintln!(
                "[beam_bifs re] option-flag oracle cross-check SKIPPED (the oracle could not \
                 answer case `{}`).",
                case.name
            );
            return;
        };

        match (&ours, &theirs) {
            (Answer::Raises, Answer::Raises) => {}
            (Answer::Hex(ours), Answer::Hex(theirs)) if ours == theirs => {}
            _ => panic!(
                "case `{}` disagrees with the oracle\n erl:    {}\n oracle: {theirs:?}\n ours:   {ours:?}",
                case.name, case.erl
            ),
        }
        ran += 1;
    }

    eprintln!(
        "[beam_bifs re] option-flag oracle cross-check RAN against '{}': {} cases agreed in {:?}.",
        oracle.display(),
        ran,
        started.elapsed(),
    );
}

// ---------------------------------------------------------------------------
// The third corpus: `re:compile/1,2` compile-error fidelity
// ---------------------------------------------------------------------------

/// The atoms [`compile_error_corpus`] needs beyond the shared [`Fixture::new`]
/// seed — the control case's `caseless` option.
const COMPILE_ERROR_ATOMS: &[&str] = &["caseless"];

/// `true` when `term` is the atom `name` in the fixture's atom table.
fn is_atom_named(f: &Fixture, term: Eterm, name: &str) -> bool {
    crate::access::is_atom(term) && f.atoms.name(beam_term::atom_val(term)) == Some(name)
}

/// Projects a `re:compile/2` result onto its **payload-free** prefix
/// `{ok, {re_pattern, NCap, UnicodeFlag, CrlfFlag}}`, asserting the tag names and
/// the 5-element arity on the way.
///
/// A whole `re:compile` result can never be compared byte-for-byte with the
/// oracle's: element 5 of the inner tuple is the compiled pattern, which the
/// oracle renders as PCRE2's opaque `#Ref<…>` and this engine as its own payload
/// image. Dropping that element leaves exactly the fields the two runtimes must
/// agree on.
fn project_re_pattern(f: &mut Fixture, out: Eterm) -> Result<Eterm> {
    use crate::access;
    if !access::is_tuple(out)
        || access::tuple_size(out) != 2
        || !is_atom_named(f, access::tuple_element(out, 0), "ok")
    {
        panic!("re:compile did not return an `{{ok, …}}` pair");
    }
    let mp = access::tuple_element(out, 1);
    if !access::is_tuple(mp)
        || access::tuple_size(mp) != 5
        || !is_atom_named(f, access::tuple_element(mp, 0), "re_pattern")
    {
        panic!("re:compile did not return a 5-element `{{re_pattern, …}}`");
    }
    let ncap = access::tuple_element(mp, 1);
    let unicode = access::tuple_element(mp, 2);
    let crlf = access::tuple_element(mp, 3);
    let tag = a(f, "re_pattern");
    let inner = t(f, &[tag, ncap, unicode, crlf]);
    let ok = a(f, "ok");
    Ok(t(f, &[ok, inner]))
}

/// The third corpus: the **compile-error** value of a malformed pattern — 17
/// cases plus one payload-free shape control.
///
/// Every `erl` expression is `re:compile(PATTERN)`. On both sides a malformed
/// pattern is a *returned* `{error, {Msg, Pos}}` value, never a raise, so the ETF
/// bytes — the PCRE2 message string **and** its 0-based byte position — are
/// byte-comparable. The diagnostic each malformed pattern pins:
///
/// * 1, 2, 11 — `missing closing parenthesis` (a group body, an unterminated
///   `(?i` option run, and `a(b` where the group never closes);
/// * 3, 4 — `missing terminating ] for character class` (an unclosed `[a`, and
///   `[]` whose leading `]` is a literal);
/// * 5, 6 — `numbers out of order in {} quantifier` (`a{2,1}`, `a{3,2}`);
/// * 7, 8, 9 — `quantifier does not follow a repeatable item` (a leading `*`, a
///   leading `{2}`, and a doubled `a**`);
/// * 10 — `\ at end of pattern`;
/// * 12 — `unmatched closing parenthesis`;
/// * 13 — `two named subpatterns have the same name (PCRE2_DUPNAMES not set)`;
/// * 14 — `range out of order in character class`;
/// * 15 — `subpattern name must start with a non-digit`;
/// * 16, 17 — the two `\x{}` diagnostics: `non-hex character in \x{} (closing
///   brace missing?)` and `character code point value in \x{} or \o{} is too
///   large`.
///
/// The final case is the **control**: `re:compile("a", [caseless])` must
/// *succeed* on both sides, so it is not byte-comparable end to end (the oracle's
/// element 5 is PCRE2's `#Ref<…>`). Its `erl` expression projects the result onto
/// the payload-free `{ok, {re_pattern, NCap, UnicodeFlag, CrlfFlag}}` prefix and
/// the Rust side does the same through [`project_re_pattern`], so the comparison
/// pins the tag, `NCap`, the unicode flag and the crlf flag — never the payload.
fn compile_error_corpus() -> Vec<Case> {
    vec![
        Case {
            name: "compile-error/a(b",
            erl: r#"re:compile("a(b")"#,
            run: |f| {
                let pattern = chr(f, "a(b");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/[a",
            erl: r#"re:compile("[a")"#,
            run: |f| {
                let pattern = chr(f, "[a");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/[]",
            erl: r#"re:compile("[]")"#,
            run: |f| {
                let pattern = chr(f, "[]");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/a{2,1}",
            erl: r#"re:compile("a{2,1}")"#,
            run: |f| {
                let pattern = chr(f, "a{2,1}");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/a{3,2}",
            erl: r#"re:compile("a{3,2}")"#,
            run: |f| {
                let pattern = chr(f, "a{3,2}");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/*a",
            erl: r#"re:compile("*a")"#,
            run: |f| {
                let pattern = chr(f, "*a");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/{2}",
            erl: r#"re:compile("{2}")"#,
            run: |f| {
                let pattern = chr(f, "{2}");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/a**",
            erl: r#"re:compile("a**")"#,
            run: |f| {
                let pattern = chr(f, "a**");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/a\\",
            erl: r#"re:compile("a\\")"#,
            run: |f| {
                let pattern = chr(f, "a\\");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/a)",
            erl: r#"re:compile("a)")"#,
            run: |f| {
                let pattern = chr(f, "a)");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/(?i",
            erl: r#"re:compile("(?i")"#,
            run: |f| {
                let pattern = chr(f, "(?i");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/duplicate-subpattern-name",
            erl: r#"re:compile("(?<n>a)(?<n>b)")"#,
            run: |f| {
                let pattern = chr(f, "(?<n>a)(?<n>b)");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/[z-a]",
            erl: r#"re:compile("[z-a]")"#,
            run: |f| {
                let pattern = chr(f, "[z-a]");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/(?<1a>x)",
            erl: r#"re:compile("(?<1a>x)")"#,
            run: |f| {
                let pattern = chr(f, "(?<1a>x)");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/hex-brace-non-hex",
            erl: r#"re:compile("\\x{zz}")"#,
            run: |f| {
                let pattern = chr(f, "\\x{zz}");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/unknown-posix-class",
            erl: r#"re:compile("[[:foo:]]")"#,
            run: |f| {
                let pattern = chr(f, "[[:foo:]]");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        Case {
            name: "compile-error/hex-brace-too-large",
            erl: r#"re:compile("\\x{110000}")"#,
            run: |f| {
                let pattern = chr(f, "\\x{110000}");
                f.call_fn("re", "compile", 1, &[pattern])
            },
        },
        // ---- the control: a compile that must SUCCEED ----------------------
        Case {
            name: "compile/control-caseless-shape",
            erl: r#"case re:compile("a",[caseless]) of
                     {ok,{re_pattern,N,U,C,_}} -> {ok,{re_pattern,N,U,C}};
                     X -> X
                   end"#,
            run: |f| {
                let pattern = chr(f, "a");
                let caseless = a(f, "caseless");
                let opts = l(f, &[caseless]);
                let out = f.call_fn("re", "compile", 2, &[pattern, opts])?;
                project_re_pattern(f, out)
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// The fourth corpus: pattern-syntax constructs through `re:run/2,3`
// ---------------------------------------------------------------------------

/// The atoms [`pattern_syntax_corpus`] needs beyond the shared [`Fixture::new`]
/// seed — the `dotall` run option (the rest — `global`, `capture`, `all`,
/// `index` — are shared).
const PATTERN_SYNTAX_ATOMS: &[&str] = &["dotall"];

/// The fourth corpus: the **pattern syntax** constructs (28 cases) exercised
/// through `re:run/2,3`, so each pins the construct's *effect* on a match and its
/// captures rather than merely that it parses.
///
/// Coverage: `.` and its `dotall` interaction (1, 2, 22); the shorthand classes
/// `\d`/`\w`/`\s` and a `[a-c]` range (3–6); the `\A`/`\z` absolute anchors vs the
/// `\b`/`\B` boundary assertions (7–9, 27); a POSIX class (10); the `\Q…\E`
/// literal run (11); a backreference (12); the `{n,}`/`{n,m}`/`+?`/`*+`
/// greedy/lazy/possessive quantifiers and a nested group repeat (13–17);
/// the `\x{…}` hex escape and `\t` (18, 24); a named group and the inline
/// `(?i)`/`(?s)` option groups, scoped and unscoped (19–22); a non-capturing
/// group (23); `^`/`$` with an alternation (25, 26); and a `global` sweep (28).
fn pattern_syntax_corpus() -> Vec<Case> {
    vec![
        Case {
            name: "syntax/dot",
            erl: r#"re:run("xabcy",".")"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b".");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/dotall-newline",
            erl: r#"re:run("a\nb","a.b",[dotall])"#,
            run: |f| {
                let subject = chr(f, "a\nb");
                let pattern = bin(f, b"a.b");
                let dotall = a(f, "dotall");
                f.run_opts(subject, pattern, &[dotall])
            },
        },
        Case {
            name: "syntax/class-range",
            erl: r#"re:run("abc","[a-c]+")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"[a-c]+");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/digit-class",
            erl: r#"re:run("abc1","\\d+")"#,
            run: |f| {
                let subject = chr(f, "abc1");
                let pattern = bin(f, br"\d+");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/word-class",
            erl: r#"re:run("abc_1","\\w+")"#,
            run: |f| {
                let subject = chr(f, "abc_1");
                let pattern = bin(f, br"\w+");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/space-class",
            erl: r#"re:run("a b","\\s")"#,
            run: |f| {
                let subject = chr(f, "a b");
                let pattern = bin(f, br"\s");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/anchor-absolute-start",
            erl: r#"re:run("abc","\\Aabc")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, br"\Aabc");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/anchor-absolute-end",
            erl: r#"re:run("abc","bc\\z")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, br"bc\z");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/word-boundary-nomatch",
            erl: r#"re:run("abc","\\bb")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, br"\bb");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/posix-class",
            erl: r#"re:run("abc","[[:alpha:]]+")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"[[:alpha:]]+");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/quoted-literal",
            erl: r#"re:run("a+b","\\Qa+b\\E")"#,
            run: |f| {
                let subject = chr(f, "a+b");
                let pattern = bin(f, br"\Qa+b\E");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/backreference",
            erl: r#"re:run("abab","(a)(b)\\1")"#,
            run: |f| {
                let subject = chr(f, "abab");
                let pattern = bin(f, br"(a)(b)\1");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/quantifier-open-range",
            erl: r#"re:run("aaa","a{2,}")"#,
            run: |f| {
                let subject = chr(f, "aaa");
                let pattern = bin(f, b"a{2,}");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/quantifier-closed-range",
            erl: r#"re:run("aaaa","a{2,3}")"#,
            run: |f| {
                let subject = chr(f, "aaaa");
                let pattern = bin(f, b"a{2,3}");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/quantifier-lazy",
            erl: r#"re:run("aaa","a+?")"#,
            run: |f| {
                let subject = chr(f, "aaa");
                let pattern = bin(f, b"a+?");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/quantifier-possessive-nomatch",
            erl: r#"re:run("aaa","a*+a")"#,
            run: |f| {
                let subject = chr(f, "aaa");
                let pattern = bin(f, b"a*+a");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/group-repeat",
            erl: r#"re:run("aaa","(a)*")"#,
            run: |f| {
                let subject = chr(f, "aaa");
                let pattern = bin(f, b"(a)*");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/hex-brace-escape",
            erl: r#"re:run("A","\\x{41}")"#,
            run: |f| {
                let subject = chr(f, "A");
                let pattern = bin(f, br"\x{41}");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/named-group",
            erl: r#"re:run("abc","(?<foo>b)(c)")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"(?<foo>b)(c)");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/inline-caseless",
            erl: r#"re:run("ABC","(?i)abc")"#,
            run: |f| {
                let subject = chr(f, "ABC");
                let pattern = bin(f, b"(?i)abc");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/scoped-caseless",
            erl: r#"re:run("abc","a(?i:BC)")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a(?i:BC)");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/inline-dotall",
            erl: r#"re:run("a\nb","(?s)a.b")"#,
            run: |f| {
                let subject = chr(f, "a\nb");
                let pattern = bin(f, b"(?s)a.b");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/non-capturing-group",
            erl: r#"re:run("abc","(?:b)(c)",[{capture,all,index}])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"(?:b)(c)");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "index");
                f.run_opts(subject, pattern, &[spec])
            },
        },
        Case {
            name: "syntax/tab-escape",
            erl: r#"re:run("a\tb","a\\tb")"#,
            run: |f| {
                let subject = chr(f, "a\tb");
                let pattern = bin(f, br"a\tb");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/anchored-alternation",
            erl: r#"re:run("abc","^(?:a|b)(c)$")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"^(?:a|b)(c)$");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/quantifier-partial-range",
            erl: r#"re:run("xaay","a{1,2}")"#,
            run: |f| {
                let subject = chr(f, "xaay");
                let pattern = bin(f, b"a{1,2}");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/non-word-boundary",
            erl: r#"re:run("abc","\\B")"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, br"\B");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "syntax/global-group-repeat",
            erl: r#"re:run("abab","(a|b)+",[global])"#,
            run: |f| {
                let subject = chr(f, "abab");
                let pattern = bin(f, b"(a|b)+");
                let global = a(f, "global");
                f.run_opts(subject, pattern, &[global])
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// The fifth corpus: subjects (the iodata flattening) and the `unicode` mode
// ---------------------------------------------------------------------------

/// The atoms [`subjects_and_unicode_corpus`] needs beyond the shared
/// [`Fixture::new`] seed — the `unicode` compile/run option (the `capture`/`all`/
/// `list`/`binary`/`offset` names the other cases use are already seeded).
const SUBJECT_UNICODE_ATOMS: &[&str] = &["unicode"];

/// The fifth corpus: **subjects** and the `unicode` mode (16 cases).
///
/// The subject is the half of `re:run/2,3` the earlier corpora barely touch (they
/// use plain charlists); each case here pins how an *iodata* subject is flattened
/// and what the reported byte offsets mean:
///
/// * 1, 2 — nested iodata (`[<<"xa">>, "bcy"]`) and the empty charlist `[]`;
/// * 3, 4 — the empty subject/pattern pair, and the `badarg` for a float element
///   (a float is not iodata);
/// * 5, 6 — the RE itself as a charlist and as a binary;
/// * 7–14 — `unicode` mode: a UTF-8 binary subject, an equivalent charlist, the
///   `list`/`binary` captures, a non-ASCII class, `.` matching one *codepoint*
///   vs one *byte* (non-unicode), and `{offset, N}` counting **bytes**;
/// * 15, 16 — an invalid-UTF-8 byte subject is `badarg` in `unicode` mode but a
///   plain one-byte subject without it.
fn subjects_and_unicode_corpus() -> Vec<Case> {
    vec![
        Case {
            name: "subject/nested-iodata",
            erl: r#"re:run([<<"xa">>,"bcy"],"b")"#,
            run: |f| {
                let inner = bin(f, b"xa");
                let tail = chr(f, "bcy");
                let subject = l(f, &[inner, tail]);
                let pattern = bin(f, b"b");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "subject/empty-charlist",
            erl: r#"re:run([],"a")"#,
            run: |f| {
                let subject = l(f, &[]);
                let pattern = bin(f, b"a");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "subject/empty-both",
            erl: r#"re:run("", "")"#,
            run: |f| {
                let subject = chr(f, "");
                let pattern = bin(f, b"");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "subject/float-is-not-iodata",
            erl: r#"re:run([1.0],"a")"#,
            run: |f| {
                let flt = crate::access::make_float(&mut f.heap, 1.0).expect("a float term");
                let subject = l(f, &[flt]);
                let pattern = bin(f, b"a");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "re/charlist",
            erl: r#"re:run("abc",[97])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = chr(f, "a");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "re/binary",
            erl: r#"re:run("abc",<<"a">>)"#,
            run: |f| {
                let subject = chr(f, "abc");
                let pattern = bin(f, b"a");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "unicode/utf8-binary-subject",
            erl: r#"re:run(<<"åäö"/utf8>>,<<"ä"/utf8>>,[unicode])"#,
            run: |f| {
                let subject = bin(f, "åäö".as_bytes());
                let pattern = bin(f, "ä".as_bytes());
                let unicode = a(f, "unicode");
                f.run_opts(subject, pattern, &[unicode])
            },
        },
        Case {
            name: "unicode/charlist-subject",
            erl: r#"re:run("åäö","ä",[unicode])"#,
            run: |f| {
                let subject = chr(f, "åäö");
                let pattern = bin(f, "ä".as_bytes());
                let unicode = a(f, "unicode");
                f.run_opts(subject, pattern, &[unicode])
            },
        },
        Case {
            name: "unicode/capture-list",
            erl: r#"re:run("åäö","ä",[unicode,{capture,all,list}])"#,
            run: |f| {
                let subject = chr(f, "åäö");
                let pattern = bin(f, "ä".as_bytes());
                let unicode = a(f, "unicode");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "list");
                f.run_opts(subject, pattern, &[unicode, spec])
            },
        },
        Case {
            name: "unicode/capture-binary",
            erl: r#"re:run("åäö","ä",[unicode,{capture,all,binary}])"#,
            run: |f| {
                let subject = chr(f, "åäö");
                let pattern = bin(f, "ä".as_bytes());
                let unicode = a(f, "unicode");
                let all = a(f, "all");
                let spec = cap_ty(f, all, "binary");
                f.run_opts(subject, pattern, &[unicode, spec])
            },
        },
        Case {
            name: "unicode/non-ascii-class",
            erl: r#"re:run("åäö","[åä]",[unicode])"#,
            run: |f| {
                let subject = chr(f, "åäö");
                let pattern = bin(f, "[åä]".as_bytes());
                let unicode = a(f, "unicode");
                f.run_opts(subject, pattern, &[unicode])
            },
        },
        Case {
            name: "unicode/dot-is-a-codepoint",
            erl: r#"re:run("åäö",".",[unicode])"#,
            run: |f| {
                let subject = chr(f, "åäö");
                let pattern = bin(f, b".");
                let unicode = a(f, "unicode");
                f.run_opts(subject, pattern, &[unicode])
            },
        },
        Case {
            name: "non-unicode/dot-is-a-byte",
            erl: r#"re:run(<<"åäö"/utf8>>,".")"#,
            run: |f| {
                let subject = bin(f, "åäö".as_bytes());
                let pattern = bin(f, b".");
                f.run_opts(subject, pattern, &[])
            },
        },
        Case {
            name: "unicode/offset-counts-bytes",
            erl: r#"re:run("åäö","ö",[unicode,{offset,4}])"#,
            run: |f| {
                let subject = chr(f, "åäö");
                let pattern = bin(f, "ö".as_bytes());
                let unicode = a(f, "unicode");
                let offset = pair(f, "offset", make_small(4));
                f.run_opts(subject, pattern, &[unicode, offset])
            },
        },
        Case {
            name: "unicode/invalid-utf8-subject",
            erl: r#"re:run(<<255>>,".",[unicode])"#,
            run: |f| {
                let subject = bin(f, &[255]);
                let pattern = bin(f, b".");
                let unicode = a(f, "unicode");
                f.run_opts(subject, pattern, &[unicode])
            },
        },
        Case {
            name: "non-unicode/raw-byte-subject",
            erl: r#"re:run(<<255>>,".")"#,
            run: |f| {
                let subject = bin(f, &[255]);
                let pattern = bin(f, b".");
                f.run_opts(subject, pattern, &[])
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// The sixth corpus: `version/0`, `inspect/2`, the precompiled run path,
// `internal_run/4`, and the `export`/`import` round-trip
// ---------------------------------------------------------------------------

/// The atoms [`run_variants_corpus`] needs beyond the shared [`Fixture::new`]
/// seed — `namelist` (`inspect/2`'s item), `caseless`/`unicode`/`export` (compile
/// options) and `zzz` (the `re:import(zzz)` control).
const RUN_VARIANTS_ATOMS: &[&str] = &["namelist", "caseless", "unicode", "export", "zzz"];

/// Compiles `pattern` (a charlist) under `opts` through the real registry and
/// returns the tuple [`re:compile`](super::compile) produced *inside* its
/// `{ok, …}` — the `{re_pattern, …}` (or `{re_exported_pattern, …}`) term the
/// precompiled cases feed to `re:run`/`re:internal_run`/`re:import`.
///
/// This is the Rust half of a **precompiled** case: the oracle's expression
/// compiles *its own* pattern, so the compiled term itself is never compared
/// (this engine's `Payload` is its own, not PCRE2's `#Ref<…>`) — only the run
/// result is. Panics only if our own `re:compile` fails, which no case here does.
fn compile_pattern(f: &mut Fixture, pattern: &str, opts: &[Eterm]) -> Eterm {
    let pat = chr(f, pattern);
    let out = if opts.is_empty() {
        f.call_fn("re", "compile", 1, &[pat])
    } else {
        let list = f.list(opts);
        f.call_fn("re", "compile", 2, &[pat, list])
    }
    .expect("our re:compile succeeds for the case's pattern and options");
    crate::access::tuple_element(out, 1)
}

/// Projects a `re:compile(P, [export])` result onto its **payload-free**
/// `{Pattern, Options}` pair — the two elements of the `{re_exported_pattern,
/// Header, Pattern, Options, Payload}` tuple the two runtimes must agree on.
///
/// The header binary and the encoded payload are implementation-private (this
/// engine's image is its own, the oracle's is PCRE2's), so they are never
/// compared; the original pattern and the option list are.
fn project_exported_pattern(f: &mut Fixture, out: Eterm) -> Result<Eterm> {
    use crate::access;
    if !access::is_tuple(out)
        || access::tuple_size(out) != 2
        || !is_atom_named(f, access::tuple_element(out, 0), "ok")
    {
        panic!("re:compile did not return an `{{ok, …}}` pair");
    }
    let exported = access::tuple_element(out, 1);
    if !access::is_tuple(exported)
        || access::tuple_size(exported) != 5
        || !is_atom_named(f, access::tuple_element(exported, 0), "re_exported_pattern")
    {
        panic!("re:compile with `export` did not return a 5-element `{{re_exported_pattern, …}}`");
    }
    let pattern = access::tuple_element(exported, 2);
    let options = access::tuple_element(exported, 3);
    Ok(t(f, &[pattern, options]))
}

/// The sixth corpus: `re:version/0`, `re:inspect/2`, the **precompiled** run
/// path, `re:internal_run/4` and the `export`/`import` round-trip (18 cases).
///
/// * 1 — `re:version()` (the modelled `re` surface version binary);
/// * 2, 3 — `re:inspect(MP, namelist)` with and without named groups;
/// * 4–6 — a **precompiled** `{re_pattern, …}` fed to `re:run/2,3` with no options,
///   with `global`, and with a flag baked into the pattern by `re:compile/2`;
/// * 7–9 — a compile-only option (`caseless`, `unicode`, `export`) alongside a
///   precompiled RE is `badarg`;
/// * 10–12 — `re:internal_run/4` on a precompiled RE (`true`/`false` first-call
///   flag, and `global`);
/// * 13, 14 — a *textual* pattern through `re:internal_run/4`, and the `badarg`
///   for a non-boolean fourth argument;
/// * 15, 16 — the `export` → `import` → `run` round-trip, and `re:import(zzz)`
///   (a `badarg`);
/// * 17 — `re:run` on the raw `export` form is `badarg` (it must be imported
///   first, exactly as the oracle refuses it);
/// * 18 — the payload-free projection of the `export` form's `{Pattern, Options}`.
///
/// Every precompiled case writes its RE as `element(2, re:compile(…))` — the
/// `{ok, MP}` `re:compile/1,2` returns is **not** itself an `mp()`; the oracle's
/// `re:run`/`re:internal_run` reject the wrapped form (`badarg`), so the pattern
/// must be unwrapped on both sides to exercise the precompiled path at all.
fn run_variants_corpus() -> Vec<Case> {
    vec![
        Case {
            name: "version/0",
            erl: r#"re:version()"#,
            run: |f| f.call_fn("re", "version", 0, &[]),
        },
        Case {
            name: "inspect/namelist-named",
            erl: r#"re:inspect(element(2,re:compile("(?<A>A)|(?<B>B)|(?<C>C)")), namelist)"#,
            run: |f| {
                let mp = compile_pattern(f, "(?<A>A)|(?<B>B)|(?<C>C)", &[]);
                let item = a(f, "namelist");
                f.call_fn("re", "inspect", 2, &[mp, item])
            },
        },
        Case {
            name: "inspect/namelist-empty",
            erl: r#"re:inspect(element(2,re:compile("(b)(c)")), namelist)"#,
            run: |f| {
                let mp = compile_pattern(f, "(b)(c)", &[]);
                let item = a(f, "namelist");
                f.call_fn("re", "inspect", 2, &[mp, item])
            },
        },
        Case {
            name: "precompiled/run2",
            erl: r#"re:run("xabcy", element(2,re:compile("(b)(c)")))"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let mp = compile_pattern(f, "(b)(c)", &[]);
                f.run_opts(subject, mp, &[])
            },
        },
        Case {
            name: "precompiled/global",
            erl: r#"re:run("abab", element(2,re:compile("a")), [global])"#,
            run: |f| {
                let subject = chr(f, "abab");
                let mp = compile_pattern(f, "a", &[]);
                let global = a(f, "global");
                f.run_opts(subject, mp, &[global])
            },
        },
        Case {
            name: "precompiled/baked-caseless",
            erl: r#"re:run("ABC", element(2,re:compile("abc", [caseless])))"#,
            run: |f| {
                let subject = chr(f, "ABC");
                let caseless = a(f, "caseless");
                let mp = compile_pattern(f, "abc", &[caseless]);
                f.run_opts(subject, mp, &[])
            },
        },
        Case {
            name: "precompiled/compile-only-caseless",
            erl: r#"re:run("abc", element(2,re:compile("a")), [caseless])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let mp = compile_pattern(f, "a", &[]);
                let caseless = a(f, "caseless");
                f.run_opts(subject, mp, &[caseless])
            },
        },
        Case {
            name: "precompiled/compile-only-unicode",
            erl: r#"re:run("abc", element(2,re:compile("a")), [unicode])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let mp = compile_pattern(f, "a", &[]);
                let unicode = a(f, "unicode");
                f.run_opts(subject, mp, &[unicode])
            },
        },
        Case {
            name: "precompiled/compile-only-export",
            erl: r#"re:run("abc", element(2,re:compile("a")), [export])"#,
            run: |f| {
                let subject = chr(f, "abc");
                let mp = compile_pattern(f, "a", &[]);
                let export = a(f, "export");
                f.run_opts(subject, mp, &[export])
            },
        },
        Case {
            name: "internal_run/offset-true",
            erl: r#"re:internal_run("xabcy", element(2,re:compile("(b)(c)")), [{offset,0}], true)"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let mp = compile_pattern(f, "(b)(c)", &[]);
                let seq = pair(f, "offset", make_small(0));
                let opts = l(f, &[seq]);
                let tru = a(f, "true");
                f.call_fn("re", "internal_run", 4, &[subject, mp, opts, tru])
            },
        },
        Case {
            name: "internal_run/offset-false",
            erl: r#"re:internal_run("xabcy", element(2,re:compile("(b)(c)")), [{offset,0}], false)"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let mp = compile_pattern(f, "(b)(c)", &[]);
                let seq = pair(f, "offset", make_small(0));
                let opts = l(f, &[seq]);
                let fals = a(f, "false");
                f.call_fn("re", "internal_run", 4, &[subject, mp, opts, fals])
            },
        },
        Case {
            name: "internal_run/global",
            erl: r#"re:internal_run("xabcy", element(2,re:compile("(b)(c)")), [global], true)"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let mp = compile_pattern(f, "(b)(c)", &[]);
                let global = a(f, "global");
                let opts = l(f, &[global]);
                let tru = a(f, "true");
                f.call_fn("re", "internal_run", 4, &[subject, mp, opts, tru])
            },
        },
        Case {
            name: "internal_run/textual-pattern",
            erl: r#"re:internal_run("xabcy", "(b)(c)", [], true)"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let opts = l(f, &[]);
                let tru = a(f, "true");
                f.call_fn("re", "internal_run", 4, &[subject, pattern, opts, tru])
            },
        },
        Case {
            name: "internal_run/non-boolean-flag",
            erl: r#"re:internal_run("xabcy", "(b)(c)", [], 1)"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let pattern = bin(f, b"(b)(c)");
                let opts = l(f, &[]);
                let one = make_small(1);
                f.call_fn("re", "internal_run", 4, &[subject, pattern, opts, one])
            },
        },
        Case {
            name: "export-import/round-trip",
            erl: r#"re:run("xabcy", re:import(element(2,re:compile("(b)(c)", [export]))))"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let export = a(f, "export");
                let exported = compile_pattern(f, "(b)(c)", &[export]);
                let mp = f.call_fn("re", "import", 1, &[exported])?;
                f.run_opts(subject, mp, &[])
            },
        },
        Case {
            name: "import/non-exported",
            erl: r#"re:import(zzz)"#,
            run: |f| {
                let zzz = a(f, "zzz");
                f.call_fn("re", "import", 1, &[zzz])
            },
        },
        Case {
            name: "run/raw-export-form-is-badarg",
            erl: r#"re:run("xabcy", element(2,re:compile("(b)(c)", [export])))"#,
            run: |f| {
                let subject = chr(f, "xabcy");
                let export = a(f, "export");
                let exported = compile_pattern(f, "(b)(c)", &[export]);
                f.run_opts(subject, exported, &[])
            },
        },
        Case {
            name: "export/projected-pattern-and-options",
            erl: r#"case re:compile("(b)(c)",[export]) of
                     {ok,{re_exported_pattern,_,P,O,_}} -> {P,O};
                     X -> X
                   end"#,
            run: |f| {
                let export = a(f, "export");
                let pattern = chr(f, "(b)(c)");
                let opts = l(f, &[export]);
                let out = f.call_fn("re", "compile", 2, &[pattern, opts])?;
                project_exported_pattern(f, out)
            },
        },
    ]
}

/// The shared sweep used by the corpus tests: run every case on our side and
/// through the real `erl`, and require the two to agree (or both to raise).
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
fn run_oracle_sweep(label: &str, cases: &[Case], atoms: &[&str]) {
    let Some(oracle) = resolve_oracle() else {
        eprintln!("[beam_bifs re] {label} oracle cross-check SKIPPED: no usable `erl` on $PATH");
        eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        return;
    };

    let mut fx = Fixture::new();
    if !atoms.is_empty() {
        fx.atoms.seed(atoms.iter().copied());
    }
    let started = Instant::now();
    let mut ran = 0usize;

    for case in cases {
        // Our side, over the real dispatch path.
        let ours = match (case.run)(&mut fx) {
            Ok(term) => match fx.encode_hex(term) {
                Ok(hex) => Answer::Hex(hex),
                Err(e) => panic!(
                    "case `{}` produced a term this crate cannot encode: {e:?}\n erl: {}",
                    case.name, case.erl
                ),
            },
            Err(_) => Answer::Raises,
        };

        // The oracle's answer for the same Erlang expression.
        let Some(theirs) = oracle_answer(&oracle, case.erl) else {
            eprintln!(
                "[beam_bifs re] {label} oracle cross-check SKIPPED (the oracle could not \
                 answer case `{}`).",
                case.name
            );
            return;
        };

        match (&ours, &theirs) {
            (Answer::Raises, Answer::Raises) => {}
            (Answer::Hex(ours), Answer::Hex(theirs)) if ours == theirs => {}
            _ => panic!(
                "case `{}` disagrees with the oracle\n erl:    {}\n oracle: {theirs:?}\n ours:   {ours:?}",
                case.name, case.erl
            ),
        }
        ran += 1;
    }

    eprintln!(
        "[beam_bifs re] {label} oracle cross-check RAN against '{}': {} cases agreed in {:?}.",
        oracle.display(),
        ran,
        started.elapsed(),
    );
}

/// The **compile-error** differential sweep over [`compile_error_corpus`] — the
/// `{error, {Msg, Pos}}` values of malformed patterns, plus the payload-free
/// `re:compile` shape control.
///
/// The fixture is the shared [`Fixture::new`] one plus [`COMPILE_ERROR_ATOMS`].
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
#[test]
fn oracle_cross_check_compile_errors() {
    run_oracle_sweep(
        "compile-error",
        &compile_error_corpus(),
        COMPILE_ERROR_ATOMS,
    );
}

/// The **pattern-syntax** differential sweep over [`pattern_syntax_corpus`] — the
/// constructs each case exercises through `re:run/2,3`.
///
/// The fixture is the shared [`Fixture::new`] one plus [`PATTERN_SYNTAX_ATOMS`].
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
#[test]
fn oracle_cross_check_pattern_syntax() {
    run_oracle_sweep("pattern-syntax", &pattern_syntax_corpus(), PATTERN_SYNTAX_ATOMS);
}

/// The **subjects & `unicode` mode** differential sweep over
/// [`subjects_and_unicode_corpus`] — how an iodata subject is flattened and what
/// the reported byte offsets mean, with and without `unicode`.
///
/// The fixture is the shared [`Fixture::new`] one plus [`SUBJECT_UNICODE_ATOMS`].
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
#[test]
fn oracle_cross_check_subjects_and_unicode() {
    run_oracle_sweep(
        "subjects-unicode",
        &subjects_and_unicode_corpus(),
        SUBJECT_UNICODE_ATOMS,
    );
}

/// The **run-variants** differential sweep over [`run_variants_corpus`] —
/// `re:version/0`, `re:inspect/2`, the **precompiled** `re:run/2,3` path,
/// `re:internal_run/4` and the `export`/`import` round-trip.
///
/// The fixture is the shared [`Fixture::new`] one plus [`RUN_VARIANTS_ATOMS`].
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH`.
#[test]
fn oracle_cross_check_run_variants() {
    run_oracle_sweep("run-variants", &run_variants_corpus(), RUN_VARIANTS_ATOMS);
}
