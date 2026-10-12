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

//! The run-side `re:*` BIFs — `re:run/2,3` and `re:internal_run/4`.
//!
//! This module is the run-side *plumbing* only: it parses the option list,
//! resolves the RE (a precompiled payload image, or a textual pattern compiled
//! here exactly as `re:compile/1,2` compiles it), flattens the subject to bytes,
//! resolves the `{offset, N}` boundary and shapes the `nomatch` /
//! `{error, match_limit}` / `{error, {compile, …}}` results. The matching itself
//! is [`exec`](super::exec); the term shaping of a **successful** match — the
//! bare `match` atom and the `{match, […]}` / `{match, [[…], …]}` forms,
//! including every `{capture, …}` value/type combination — lives in
//! [`render`](super::render), which this module calls exactly once per run.
//!
//! The option *grammar* (and its `badarg` cases) is
//! [`options::parse`](super::options::parse); the compile-error *text* is
//! [`super::compile_error`], reused so the run side reports byte-identical
//! `{Msg, Pos}` positions to `re:compile/1,2`. This module adds no pattern
//! syntax, no option and no error of its own — every rejection below is an
//! oracle-observed `badarg`, and anything the engine cannot do stays the typed
//! `unsupported_bif` capability gap rather than a fabricated result.

use beam_common::{BeamError, Result};
use beam_term::{Eterm, NIL};

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;

use super::exec::{self, Subject};
use super::options::{self, CapType, CapValue, Parsed};
use super::prog::{self, Program};
use super::render;

/// The bare atom a failed match reports (never a `{match, []}`).
const NOMATCH: &str = "nomatch";

/// The tag of a raised/value error result.
const ERROR: &str = "error";

/// The `{match_limit, N}` budget's exhaustion reason.
const MATCH_LIMIT: &str = "match_limit";

/// The tag wrapping a compile failure under `report_errors`.
const COMPILE: &str = "compile";

/// `re:run/2` — `re:run/3` with an empty option list.
pub(crate) fn run_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    run(ctx, args[0], args[1], NIL)
}

/// `re:run/3` — match `Subject` against `RE` under `Options`.
pub(crate) fn run_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    run(ctx, args[0], args[1], args[2])
}

/// `re:internal_run/4` — the same match as `re:run/3`, with `erlang`'s internal
/// "first call" flag.
///
/// `FirstCall` must be the atom `true` or `false`; any other term is `badarg`
/// (oracle-verified: `re:internal_run("xabcy","(b)(c)",[],42)` is `badarg`). The
/// flag corresponds to PCRE2's `NO_UTF_CHECK`, which tells the matcher that the
/// subject's UTF-8 validity has already been checked by the caller — this engine
/// validates the subject itself, so the value has **no** behavioural effect here
/// and both atoms produce the same result (pinned by the unit tests). A textual
/// `RE` is accepted, exactly as for `re:run/2,3`.
pub(crate) fn internal_run_4(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let first_call = args[3];
    let is_boolean = super::is_atom_named(ctx, first_call, "true")
        || super::is_atom_named(ctx, first_call, "false");
    if !is_boolean {
        return Err(badarg());
    }
    run(ctx, args[0], args[1], args[2])
}

/// The shared `re:run/2,3` / `re:internal_run/4` body.
///
/// # Errors
///
/// [`BeamError::BadArg`] for every shape the oracle refuses: a malformed option
/// list (raised by [`options::parse`]), the compile-only `export` option, a
/// compile-only option alongside a precompiled `RE`, a malformed or non-iodata
/// subject (or a non-iodata textual `RE`), an `{offset, N}` past the subject or
/// inside a multi-byte codepoint, and a malformed pattern when `report_errors`
/// is absent. A construct outside the supported subset stays the typed
/// `unsupported_bif`; heap exhaustion propagates from the term constructors.
fn run(
    ctx: &mut BifContext<'_>,
    subject_term: Eterm,
    re_term: Eterm,
    options_term: Eterm,
) -> Result<Eterm> {
    // The option list first: a malformed one is `badarg` before anything else is
    // touched (an unknown option, a malformed `{capture, …}`, an improper list).
    let parsed = options::parse(ctx.atoms(), options_term)?;
    // `export` is compile-only — the oracle's `PARSE_FLAG_EXPORT` refusal in
    // `run` (`re:run("abc","a",[export])` is `badarg`).
    if parsed.export {
        return Err(badarg());
    }
    // The RE is resolved *before* the subject, as the oracle does: with
    // `report_errors` a compile failure is a returned value, so
    // `re:run(123,"a(b",[report_errors])` is `{error,{compile,…}}` and not the
    // subject's `badarg`.
    let (program, unicode) = match resolve_re(ctx, re_term, &parsed)? {
        Resolved::Matcher(program, unicode) => (program, unicode),
        Resolved::CompileError(value) => return Ok(value),
    };
    // The subject's iodata walk. Under `unicode` the pattern's own flag governs
    // (see `resolve_re`), so a precompiled unicode pattern gets a UTF-8 subject
    // even though a run-side `unicode` option would have been refused.
    let mut bytes: Vec<u8> = Vec::new();
    super::flatten(subject_term, unicode, &mut bytes)?;
    let subject = if unicode {
        // An invalid UTF-8 subject is the oracle's `badarg` (there is no
        // "match the raw bytes" fallback in `unicode` mode).
        Subject::utf8(bytes)?
    } else {
        Subject::latin1(bytes)
    };
    let start = start_index(&subject, parsed.start_offset)?;
    // No `{capture, …}` means the default `all`/`index` specification.
    let (value, ty) = parsed.capture.clone().unwrap_or((CapValue::All, CapType::Index));
    let ids = render::resolve_ids(&value, &program);
    let mut parsed = parsed;
    parsed.start_offset = start;
    match exec::run(&program, &subject, &parsed, &ids) {
        exec::Outcome::Match(matches) => {
            render::render(ctx, &subject, &matches, &ids, ty, parsed.global)
        }
        // Nothing matched: the bare `nomatch`, never a `{match, []}`.
        exec::Outcome::NoMatch => ctx.intern_atom(NOMATCH),
        exec::Outcome::MatchLimit => {
            if parsed.report_errors {
                let tag = ctx.intern_atom(ERROR)?;
                let reason = ctx.intern_atom(MATCH_LIMIT)?;
                ctx.tuple(&[tag, reason])
            } else {
                // Without `report_errors` an exhausted budget is indistinguishable
                // from "nothing matched" (the oracle's `nomatch`).
                ctx.intern_atom(NOMATCH)
            }
        }
    }
}

/// What [`resolve_re`] found: the compiled program (plus the **effective**
/// `unicode` flag for the subject), or the `{error, {compile, …}}` *value* a
/// `report_errors` run returns instead of raising.
enum Resolved {
    /// A compiled pattern to match with.
    Matcher(Program, bool),
    /// The value `re:run` returns for a malformed pattern under `report_errors`.
    CompileError(Eterm),
}

/// Resolves the `RE` argument to a compiled [`Program`] plus the **effective**
/// `unicode` flag for the subject.
///
/// Two forms are accepted, mirroring the oracle:
///
/// * a **precompiled** pattern tuple — [`super::pattern_image`] recognises the
///   `{re_pattern, …}` payload; the program is read straight out of the image
///   (no recompilation) and its own flag governs the subject, because the run
///   option list cannot carry `unicode` here (see below);
/// * anything else is a **textual** pattern, flattened and compiled with exactly
///   the calls [`super::compile`](super::compile) makes, so the program — and
///   therefore the match — is identical to `re:compile/1,2`'s for the same input
///   and options.
///
/// A precompiled `RE` plus **any** compile-only option is `badarg` (the oracle's
/// `PARSE_FLAG_UNIQUE_COMPILE_OPT` refusal: `re:run("abc",MP,[caseless])` is
/// `badarg`), which is why the subject's codepoint view must come from the
/// pattern's payload rather than from the option list.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a compile-only option with a precompiled `RE`, a
/// payload image that does not decode, a non-iodata textual pattern or an
/// out-of-range pattern element, and (via [`compile_failure`]) the `badarg`
/// raise that replaces an unreported malformed pattern.
fn resolve_re(ctx: &mut BifContext<'_>, re_term: Eterm, parsed: &Parsed) -> Result<Resolved> {
    if let Some(image) = super::pattern_image(ctx, re_term) {
        if parsed.unique_compile {
            return Err(badarg());
        }
        // A foreign image is the oracle's `badarg`, never a panic.
        let payload = prog::Payload::decode(&image).map_err(|_| badarg())?;
        let unicode = payload.program.flags.unicode;
        return Ok(Resolved::Matcher(payload.program, unicode));
    }
    let flags = &parsed.compile;
    // A non-iodata `RE` (`re:run("abc",123)`) is `badarg` here.
    let (codes, offs, source) = super::pattern_form(re_term, flags.unicode)?;
    let syntax = match super::syntax::parse(&codes, &offs, source.len(), flags) {
        Ok(syntax) => syntax,
        Err(err) => return Ok(Resolved::CompileError(compile_failure(ctx, err, parsed)?)),
    };
    // The only other failure is the typed `unsupported_bif` for a construct
    // outside the subset (an unrollable quantifier) — never folded into `badarg`.
    let program = prog::compile(&syntax, flags)?;
    Ok(Resolved::Matcher(program, flags.unicode))
}

/// The run side of a pattern that failed to compile.
///
/// A **malformed** pattern is a returned value under `report_errors` and a
/// `badarg` raise without it: `re:run("abc","a(b",[report_errors])` is
/// `{error,{compile,{"missing closing parenthesis",3}}}` and
/// `re:run("abc","a(b")` is `badarg`. The inner `{Msg, Pos}` pair is
/// [`super::compile_error`]'s, so the position and message are byte-identical to
/// `re:compile/2`'s for the same pattern — this module only wraps it in the
/// outer `{error, {compile, …}}`.
///
/// A failure that carries no malformed-pattern reason (an unsupported
/// construct) is **not** a compile error and propagates unchanged as the typed
/// `unsupported_bif`, with or without `report_errors`.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a malformed pattern without `report_errors`; the
/// typed gap otherwise.
fn compile_failure(ctx: &mut BifContext<'_>, err: BeamError, parsed: &Parsed) -> Result<Eterm> {
    // `Err` here means "not a malformed pattern" — the typed capability gap.
    let value = super::compile_error(ctx, err)?;
    if !parsed.report_errors {
        return Err(badarg());
    }
    // `compile_error` built `{error, {Msg, Pos}}`; the run form nests that inner
    // pair under the `compile` tag.
    let pair = access::tuple_element(value, 1);
    let tag = ctx.intern_atom(COMPILE)?;
    let inner = ctx.tuple(&[tag, pair])?;
    let outer = ctx.intern_atom(ERROR)?;
    ctx.tuple(&[outer, inner])
}

/// The matcher's start position for the `{offset, N}` option.
///
/// `N` is a **byte** offset into the flattened subject (PCRE2's `startoffset`),
/// while [`exec`] works in codepoint positions. The two coincide in the
/// non-`unicode` mode, where one codepoint is one byte, and diverge otherwise,
/// so `N` is translated through [`Subject::offs`] (the byte offset of each
/// codepoint). A `N` past the subject, or landing **inside** a multi-byte
/// codepoint, is the oracle's `badarg`: oracle-verified,
/// `re:run("ác","c",[unicode,{offset,2}])` is `{match,[{2,1}]}`,
/// `[{offset,1}]` is `badarg`, `[{offset,3}]` is `nomatch`, and
/// `re:run("abc","b",[{offset,5}])` is `badarg` while `[{offset,3}]` is
/// `nomatch`.
///
/// # Errors
///
/// [`BeamError::BadArg`] for an offset past `subject.bytes.len()` or one that is
/// not a codepoint boundary.
fn start_index(subject: &Subject, offset: u32) -> Result<u32> {
    let offset = offset as usize;
    if offset > subject.bytes.len() {
        return Err(badarg());
    }
    let index = subject.offs.binary_search(&offset).map_err(|_| badarg())?;
    // A codepoint is at least one byte, so `index <= offset` and the conversion
    // cannot fail for an offset the option parser already accepted as a `u32`.
    u32::try_from(index).map_err(|_| badarg())
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{compare, make_atom, make_small, AtomTable};

    use crate::bits;
    use crate::registry::BifRegistry;

    /// A registry + heap + seeded atom table the tests drive the `re` BIFs
    /// through (the same shape `../mod.rs`'s fixture has).
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
                "report_errors",
                "caseless",
                "multiline",
                "extended",
                "export",
                "anchored",
                "notempty",
                // A stand-in for an unknown option / a non-boolean 4th argument.
                "nope",
            ]);
            Fixture {
                registry: crate::registry::default_registry(),
                heap: Heap::new(1 << 16).expect("test heap"),
                atoms,
            }
        }

        fn atom(&self, name: &str) -> Eterm {
            make_atom(self.atoms.lookup(name).expect("seeded"))
        }

        fn binary(&mut self, bytes: &[u8]) -> Eterm {
            bits::make_binary(&mut self.heap, bytes).expect("the binary fits")
        }

        /// A charlist of `s`'s codepoints.
        fn charlist(&mut self, s: &str) -> Eterm {
            let elems: Vec<Eterm> =
                s.chars().map(|c| make_small(i64::from(c as u32))).collect();
            self.list(&elems)
        }

        fn list(&mut self, elems: &[Eterm]) -> Eterm {
            let mut tail = NIL;
            for &elem in elems.iter().rev() {
                tail = self.heap.cons(elem, tail).expect("the list fits");
            }
            tail
        }

        fn tuple(&mut self, elems: &[Eterm]) -> Eterm {
            self.heap.tuple(elems).expect("the tuple fits")
        }

        fn elem(&self, tuple: Eterm, i: usize) -> Eterm {
            access::tuple_element(tuple, i)
        }

        fn eq(&self, a: Eterm, b: Eterm) -> bool {
            compare::eq(a, b, &self.atoms)
        }

        fn call_fn(&mut self, func: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            self.registry.call("re", func, arity, &mut ctx, args)
        }

        /// `re:run/2,3` on already-built terms (`run/2` when `opts` is empty).
        fn run_terms(&mut self, subject: Eterm, re: Eterm, opts: &[Eterm]) -> Result<Eterm> {
            if opts.is_empty() {
                self.call_fn("run", 2, &[subject, re])
            } else {
                let list = self.list(opts);
                self.call_fn("run", 3, &[subject, re, list])
            }
        }

        /// `re:run/2,3` on a charlist subject and a binary pattern.
        fn run(&mut self, subject: &str, pattern: &str, opts: &[Eterm]) -> Result<Eterm> {
            let subject = self.charlist(subject);
            let pattern = self.binary(pattern.as_bytes());
            self.run_terms(subject, pattern, opts)
        }

        /// `re:internal_run/4`.
        fn internal_run(
            &mut self,
            subject: Eterm,
            re: Eterm,
            opts: &[Eterm],
            first_call: Eterm,
        ) -> Result<Eterm> {
            let list = self.list(opts);
            self.call_fn("internal_run", 4, &[subject, re, list, first_call])
        }

        /// The `{re_pattern, …}` term `re:compile/2` produces for `pattern`.
        fn compile(&mut self, pattern: &str, opts: &[Eterm]) -> Eterm {
            let pattern = self.binary(pattern.as_bytes());
            let list = self.list(opts);
            let out = self.call_fn("compile", 2, &[pattern, list]).expect("compiles");
            self.elem(out, 1)
        }

        /// Renders `term` with the `~p` printer, for readable assertions.
        fn show(&mut self, term: Eterm) -> String {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            crate::print::format_term(&mut ctx, term, true).expect("the term is printable")
        }
    }

    #[test]
    fn run_two_matches_the_oracle_shape() {
        let mut f = Fixture::new();
        // `re:run("xabcy","b")` = {match,[{2,1}]}
        let out = f.run("xabcy", "b", &[]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1}]}");

        // `re:run("xabcy","(b)(c)")` = {match,[{2,2},{2,1},{3,1}]}
        let out = f.run("xabcy", "(b)(c)", &[]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,2},{2,1},{3,1}]}");

        // A binary subject is accepted too (`re:run(<<"abc">>,"b")`).
        let subject = f.binary(b"xabcy");
        let pattern = f.binary(b"b");
        let out = f.run_terms(subject, pattern, &[]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1}]}");
    }

    #[test]
    fn a_none_capture_is_the_bare_match_atom() {
        let mut f = Fixture::new();
        // `re:run("xabcy","(b)(c)",[{capture,none}])` = match
        let capture = f.atom("capture");
        let none = f.atom("none");
        let spec = f.tuple(&[capture, none]);
        let out = f.run("xabcy", "(b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), "match");

        // The empty list is `none` as well.
        let empty = f.list(&[]);
        let spec = f.tuple(&[capture, empty]);
        let out = f.run("xabcy", "(b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), "match");

        // ... and a `global` run with no captures is still the bare atom.
        let global = f.atom("global");
        let spec = f.tuple(&[capture, none]);
        let out = f.run("abab", "a", &[global, spec]).expect("matches");
        assert_eq!(f.show(out), "match");
    }

    #[test]
    fn a_global_run_nests_one_list_per_match() {
        let mut f = Fixture::new();
        // `re:run("abab","a",[global])` = {match,[[{0,1}],[{2,1}]]}
        let global = f.atom("global");
        let out = f.run("abab", "a", &[global]).expect("matches");
        assert_eq!(f.show(out), "{match,[[{0,1}],[{2,1}]]}");

        // `re:run("abab","(a)(b)",[global,{capture,all,index}])`
        //   = {match,[[{0,2},{0,1},{1,1}],[{2,2},{2,1},{3,1}]]}
        let capture = f.atom("capture");
        let all = f.atom("all");
        let index = f.atom("index");
        let spec = f.tuple(&[capture, all, index]);
        let out = f.run("abab", "(a)(b)", &[global, spec]).expect("matches");
        assert_eq!(f.show(out), "{match,[[{0,2},{0,1},{1,1}],[{2,2},{2,1},{3,1}]]}");
    }

    #[test]
    fn the_capture_type_selects_list_or_binary() {
        let mut f = Fixture::new();
        let capture = f.atom("capture");
        let all = f.atom("all");

        // `re:run("xabcy","(b)(c)",[{capture,all,list}])` = {match,["bc","b","c"]}
        let list = f.atom("list");
        let spec = f.tuple(&[capture, all, list]);
        let out = f.run("xabcy", "(b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), r#"{match,["bc","b","c"]}"#);

        // `re:run("xabcy","(b)(c)",[{capture,all,binary}])`
        let binary = f.atom("binary");
        let spec = f.tuple(&[capture, all, binary]);
        let out = f.run("xabcy", "(b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), r#"{match,[<<"bc">>,<<"b">>,<<"c">>]}"#);
    }

    #[test]
    fn all_but_first_and_all_names_resolve_against_the_program() {
        let mut f = Fixture::new();
        let capture = f.atom("capture");
        let index = f.atom("index");

        // `re:run("xabcy","(b)(c)",[{capture,all_but_first,index}])`
        //   = {match,[{2,1},{3,1}]}
        let all_but_first = f.atom("all_but_first");
        let spec = f.tuple(&[capture, all_but_first, index]);
        let out = f.run("xabcy", "(b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1},{3,1}]}");

        // `re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,index}])`
        //   = {match,[{2,1}]} — only the *named* group.
        let all_names = f.atom("all_names");
        let spec = f.tuple(&[capture, all_names, index]);
        let out = f.run("xabcy", "(?<foo>b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1}]}");

        // ... and a requested *name* no group carries renders non-participating.
        let c = f.binary(b"c");
        let names = f.list(&[c]);
        let spec = f.tuple(&[capture, names, index]);
        let out = f.run("xabcy", "(?<foo>b)(c)", &[spec]).expect("matches");
        assert_eq!(f.show(out), "{match,[{-1,0}]}");
    }

    #[test]
    fn a_non_participating_group_renders_its_empty_form() {
        let mut f = Fixture::new();
        // `re:run("xabcy","(z)?(b)",[{capture,all,index}])`
        //   = {match,[{2,1},{-1,0},{2,1}]}
        let capture = f.atom("capture");
        let all = f.atom("all");
        let index = f.atom("index");
        let spec = f.tuple(&[capture, all, index]);
        let out = f.run("xabcy", "(z)?(b)", &[spec]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1},{-1,0},{2,1}]}");
    }

    #[test]
    fn the_offset_option_counts_bytes() {
        let mut f = Fixture::new();
        let offset = f.atom("offset");

        // `re:run("abcabc","b",[{offset,2}])` = {match,[{4,1}]}
        let two = make_small(2);
        let spec = f.tuple(&[offset, two]);
        let out = f.run("abcabc", "b", &[spec]).expect("matches");
        assert_eq!(f.show(out), "{match,[{4,1}]}");

        // `[{offset,3}]` is a legal past-the-end start: `nomatch`, not `badarg`.
        let at_end = f.tuple(&[offset, make_small(3)]);
        let out = f.run("abc", "b", &[at_end]).expect("a legal offset");
        assert_eq!(f.show(out), "nomatch");

        // `re:run("abc","b",[{offset,5}])` is badarg.
        let past = f.tuple(&[offset, make_small(5)]);
        assert!(matches!(f.run("abc", "b", &[past]), Err(BeamError::BadArg)));
    }

    #[test]
    fn a_failed_match_is_the_bare_nomatch() {
        let mut f = Fixture::new();
        // `re:run("abc","z")` = nomatch
        let out = f.run("abc", "z", &[]).expect("no match is a value");
        assert_eq!(f.show(out), "nomatch");

        // An empty subject and the empty pattern: the empty match at 0.
        let out = f.run("", "", &[]).expect("the empty pattern matches");
        assert_eq!(f.show(out), "{match,[{0,0}]}");

        // `re:run("","",[notempty,{capture,all,index}])` = nomatch
        let notempty = f.atom("notempty");
        let capture = f.atom("capture");
        let all = f.atom("all");
        let index = f.atom("index");
        let spec = f.tuple(&[capture, all, index]);
        let out = f.run("", "", &[notempty, spec]).expect("notempty refuses it");
        assert_eq!(f.show(out), "nomatch");
    }

    #[test]
    fn a_possessive_quantifier_does_not_backtrack() {
        let mut f = Fixture::new();
        // `re:run("aaa","a*+a")` = nomatch
        let out = f.run("aaa", "a*+a", &[]).expect("a value");
        assert_eq!(f.show(out), "nomatch");
        // ... while the greedy form matches all three.
        let out = f.run("aaa", "a*", &[]).expect("matches");
        assert_eq!(f.show(out), "{match,[{0,3}]}");
    }

    #[test]
    fn a_malformed_pattern_is_badarg_or_the_compile_error_value() {
        let mut f = Fixture::new();
        // `re:run("abc","a(b")` = badarg (a raise, not a value).
        assert!(matches!(f.run("abc", "a(b", &[]), Err(BeamError::BadArg)));

        // `re:run("abc","a(b",[report_errors])` = the compile error *value*,
        // with the identical `{Msg, Pos}` `re:compile/2` reports.
        let report_errors = f.atom("report_errors");
        let out = f.run("abc", "a(b", &[report_errors]).expect("a value, not a raise");
        assert_eq!(f.show(out), r#"{error,{compile,{"missing closing parenthesis",3}}}"#);

        let out = f.run("abc", "a{2,1}", &[report_errors]).expect("a value");
        assert_eq!(f.show(out), r#"{error,{compile,{"numbers out of order in {} quantifier",5}}}"#);

        // The RE is resolved before the subject: a *reported* compile failure is
        // a value even though the subject would itself be `badarg`.
        let int = make_small(123);
        let pattern = f.binary(b"a(b");
        let list = f.list(&[report_errors]);
        let out = f.call_fn("run", 3, &[int, pattern, list]).expect("a value");
        assert_eq!(f.show(out), r#"{error,{compile,{"missing closing parenthesis",3}}}"#);
    }

    #[test]
    fn a_non_iodata_subject_or_re_is_badarg() {
        let mut f = Fixture::new();
        // `re:run("abc",123)` = badarg
        let int = make_small(123);
        let subject = f.charlist("abc");
        assert!(matches!(f.run_terms(subject, int, &[]), Err(BeamError::BadArg)));

        // `re:run(123,"a")` = badarg
        let int = make_small(123);
        let pattern = f.binary(b"a");
        assert!(matches!(f.run_terms(int, pattern, &[]), Err(BeamError::BadArg)));

        // A float, an atom, a tuple and an improper list are not iodata either.
        let pattern = f.binary(b"a");
        let atom = f.atom("nope");
        assert!(matches!(f.run_terms(atom, pattern, &[]), Err(BeamError::BadArg)));
        let pattern = f.binary(b"a");
        let tuple = f.tuple(&[atom]);
        assert!(matches!(f.run_terms(tuple, pattern, &[]), Err(BeamError::BadArg)));
        let pattern = f.binary(b"a");
        let improper = f.heap.cons(make_small(97), make_small(98)).expect("fits");
        assert!(matches!(f.run_terms(improper, pattern, &[]), Err(BeamError::BadArg)));
        // An out-of-range element in a non-unicode subject.
        let pattern = f.binary(b"a");
        let big = f.list(&[make_small(256)]);
        assert!(matches!(f.run_terms(big, pattern, &[]), Err(BeamError::BadArg)));
    }

    #[test]
    fn a_precompiled_pattern_runs_and_refuses_compile_only_options() {
        let mut f = Fixture::new();
        // `re:run("xabcy", re:compile("(b)(c)")-term)` = {match,[{2,2},{2,1},{3,1}]}
        let mp = f.compile("(b)(c)", &[]);
        let subject = f.charlist("xabcy");
        let out = f.run_terms(subject, mp, &[]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,2},{2,1},{3,1}]}");

        // A precompiled RE plus *any* compile-only option is badarg.
        let caseless = f.atom("caseless");
        let subject = f.charlist("xabcy");
        assert!(matches!(f.run_terms(subject, mp, &[caseless]), Err(BeamError::BadArg)));
        let unicode = f.atom("unicode");
        let subject = f.charlist("xabcy");
        assert!(matches!(f.run_terms(subject, mp, &[unicode]), Err(BeamError::BadArg)));
        let export = f.atom("export");
        let subject = f.charlist("xabcy");
        assert!(matches!(f.run_terms(subject, mp, &[export]), Err(BeamError::BadArg)));

        // A run option alongside a precompiled RE is fine.
        let global = f.atom("global");
        let subject = f.charlist("xabcy");
        let out = f.run_terms(subject, mp, &[global]).expect("matches");
        assert_eq!(f.show(out), "{match,[[{2,2},{2,1},{3,1}]]}");

        // A tuple carrying the tag but a foreign image is badarg, not a panic.
        let tag = f.atom("re_pattern");
        let junk = f.binary(b"not an image");
        let zero = make_small(0);
        let fake = f.tuple(&[tag, zero, zero, zero, junk]);
        let subject = f.charlist("abc");
        assert!(matches!(f.run_terms(subject, fake, &[]), Err(BeamError::BadArg)));
    }

    #[test]
    fn a_compile_only_option_governs_a_textual_pattern() {
        let mut f = Fixture::new();
        // `re:run("ABC","abc",[caseless])` = {match,[{0,3}]}
        let caseless = f.atom("caseless");
        let out = f.run("ABC", "abc", &[caseless]).expect("matches");
        assert_eq!(f.show(out), "{match,[{0,3}]}");

        // The run-side compile is identical to `re:compile/1,2`'s: the *same*
        // precompiled term matches where the textual run does.
        let caseless = f.atom("caseless");
        let mp = f.compile("abc", &[caseless]);
        let subject = f.charlist("ABC");
        let out = f.run_terms(subject, mp, &[]).expect("the same match");
        assert_eq!(f.show(out), "{match,[{0,3}]}");
    }

    #[test]
    fn the_multiline_and_extended_options_apply() {
        let mut f = Fixture::new();
        // `re:run("a\nb","b$",[multiline])` = {match,[{2,1}]}
        let multiline = f.atom("multiline");
        let out = f.run("a\nb", "b$", &[multiline]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1}]}");
        // Without `multiline` the `$` only matches at the very end.
        let out = f.run("a\nb", "a$", &[]).expect("no match");
        assert_eq!(f.show(out), "nomatch");

        // `re:run("abc","a  b  c",[extended])` = {match,[{0,3}]}
        let extended = f.atom("extended");
        let out = f.run("abc", "a  b  c", &[extended]).expect("matches");
        assert_eq!(f.show(out), "{match,[{0,3}]}");
    }

    #[test]
    fn the_match_limit_needs_report_errors_for_a_value() {
        let mut f = Fixture::new();
        let report_errors = f.atom("report_errors");
        let match_limit = f.atom("match_limit");
        let zero = make_small(0);

        // `re:run("abc","a",[{match_limit,0},report_errors])` = {error,match_limit}
        let spec = f.tuple(&[match_limit, zero]);
        let out = f.run("abc", "a", &[spec, report_errors]).expect("an error value");
        assert_eq!(f.show(out), "{error,match_limit}");

        // The same budget without `report_errors` is indistinguishable from
        // "nothing matched".
        let spec = f.tuple(&[match_limit, zero]);
        let out = f.run("abc", "a", &[spec]).expect("a value");
        assert_eq!(f.show(out), "nomatch");
    }

    #[test]
    fn the_export_option_is_compile_only() {
        let mut f = Fixture::new();
        // `re:run("abc","a",[export])` = badarg
        let export = f.atom("export");
        assert!(matches!(f.run("abc", "a", &[export]), Err(BeamError::BadArg)));
    }

    #[test]
    fn internal_run_accepts_only_a_boolean_first_call() {
        let mut f = Fixture::new();
        let tru = f.atom("true");
        let fals = f.atom("false");
        let offset = f.atom("offset");
        let zero = make_small(0);
        let spec = f.tuple(&[offset, zero]);

        let subject = f.charlist("xabcy");
        let pattern = f.binary(b"(b)(c)");
        let with_true = f.internal_run(subject, pattern, &[spec], tru).expect("matches");
        assert_eq!(f.show(with_true), "{match,[{2,2},{2,1},{3,1}]}");

        let subject = f.charlist("xabcy");
        let pattern = f.binary(b"(b)(c)");
        let with_false = f.internal_run(subject, pattern, &[spec], fals).expect("matches");
        assert_eq!(f.show(with_false), "{match,[{2,2},{2,1},{3,1}]}");
        assert!(f.eq(with_true, with_false), "the flag has no behavioural effect");

        // ... and it agrees with `re:run/3` under the same options.
        let subject = f.charlist("xabcy");
        let pattern = f.binary(b"(b)(c)");
        let list = f.list(&[spec]);
        let via_run = f.call_fn("run", 3, &[subject, pattern, list]).expect("matches");
        assert!(f.eq(via_run, with_true), "internal_run and run agree");

        // A precompiled RE works on this path too.
        let mp = f.compile("(b)(c)", &[]);
        let subject = f.charlist("xabcy");
        let out = f.internal_run(subject, mp, &[], tru).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,2},{2,1},{3,1}]}");

        // A non-boolean fourth argument is badarg.
        let subject = f.charlist("xabcy");
        let pattern = f.binary(b"(b)(c)");
        let int = make_small(42);
        assert!(matches!(f.internal_run(subject, pattern, &[], int), Err(BeamError::BadArg)));
        let subject = f.charlist("xabcy");
        let pattern = f.binary(b"(b)(c)");
        let nope = f.atom("nope");
        assert!(matches!(f.internal_run(subject, pattern, &[], nope), Err(BeamError::BadArg)));
    }

    #[test]
    fn a_unicode_subject_is_flattened_and_its_offsets_are_bytes() {
        let mut f = Fixture::new();
        let unicode = f.atom("unicode");

        // A charlist of codepoints is encoded to UTF-8: `é` is two bytes, so its
        // match reports the two-byte span.
        let out = f.run("é", r"\x{e9}", &[unicode]).expect("matches");
        assert_eq!(f.show(out), "{match,[{0,2}]}");

        // An already-UTF-8 binary subject agrees.
        let subject = f.binary("é".as_bytes());
        let pattern = f.binary(r"\x{e9}".as_bytes());
        let list = f.list(&[unicode]);
        let out = f.call_fn("run", 3, &[subject, pattern, list]).expect("matches");
        assert_eq!(f.show(out), "{match,[{0,2}]}");

        // A binary that is not valid UTF-8 is badarg in `unicode` mode.
        let subject = f.binary(&[233]);
        let pattern = f.binary(r"\x{e9}".as_bytes());
        let list = f.list(&[unicode]);
        assert!(matches!(f.call_fn("run", 3, &[subject, pattern, list]), Err(BeamError::BadArg)));

        // The subject's codepoint view comes from the *pattern* when the RE is
        // precompiled: a run-side `unicode` would be refused, but a pattern
        // compiled with it still gets a UTF-8 subject.
        let mp = f.compile(r"\x{e9}", &[unicode]);
        let subject = f.charlist("é");
        let out = f.run_terms(subject, mp, &[]).expect("matches");
        assert_eq!(f.show(out), "{match,[{0,2}]}");
        let subject = f.binary(&[233]);
        assert!(matches!(f.run_terms(subject, mp, &[]), Err(BeamError::BadArg)));

        // `{offset, N}` counts bytes: 2 is one past the two-byte `á` (so it lands
        // on `c`), 1 is *inside* it (badarg), and 3 is past the end of `ác`
        // (nomatch). `unicode` makes the charlist a UTF-8 subject.
        let offset = f.atom("offset");
        let subject = f.charlist("ác");
        let pattern = f.binary(b"c");
        let spec = f.tuple(&[offset, make_small(2)]);
        let list = f.list(&[unicode, spec]);
        let out = f.call_fn("run", 3, &[subject, pattern, list]).expect("matches");
        assert_eq!(f.show(out), "{match,[{2,1}]}");

        let subject = f.charlist("ác");
        let pattern = f.binary(b"c");
        let spec = f.tuple(&[offset, make_small(1)]);
        let list = f.list(&[unicode, spec]);
        assert!(matches!(f.call_fn("run", 3, &[subject, pattern, list]), Err(BeamError::BadArg)));

        let subject = f.charlist("ác");
        let pattern = f.binary(b"c");
        let spec = f.tuple(&[offset, make_small(3)]);
        let list = f.list(&[unicode, spec]);
        let out = f.call_fn("run", 3, &[subject, pattern, list]).expect("a legal offset");
        assert_eq!(f.show(out), "nomatch");
    }
}
