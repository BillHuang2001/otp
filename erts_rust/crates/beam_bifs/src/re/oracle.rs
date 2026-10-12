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
