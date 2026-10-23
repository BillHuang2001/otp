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

//! **The shared oracle-corpus machinery** — the single implementation of the
//! corpus parsing, term building, `~w` rendering and outcome comparison that
//! every suite asserting an engine result against the **real system `erl`**
//! drives.
//!
//! Two corpora live in `tests/fixtures/`, and both are *data*: every expectation
//! in them was produced by the system `erl` (OTP 29 / erts-17.1, the differential
//! oracle) and each line quotes the exact `erl -noshell -eval …` command that
//! reproduces it. This module is the code half:
//!
//! * [`parse_breadth_target`] / [`breadth_args`] — the happy-path corpus
//!   (`<module>:<function>(<args>) => <expected>`), whose expected column is the
//!   verbatim `~w` rendering the oracle printed. [`parse_expected_value`] turns
//!   that column back into a **term** so a run can be compared structurally
//!   (`beam_term::cmp` — the engine's own `=:=`), and [`render_term`] renders a
//!   term back into the corpus's spelling for diagnostics.
//! * [`parse_error_targets`] / [`verify_error`] — the error-path corpus
//!   (`<module>:<function>/<arity> <poisons> => <expected>`), whose expected
//!   column is the *exception* the oracle raises. [`ErrorVerdict`] separates the
//!   three honest outcomes: the engine reproduced the oracle's class and reason
//!   term ([`ErrorVerdict::Matches`]), the engine could not execute the raise at
//!   all and answered a typed gap ([`ErrorVerdict::TypedGap`] — counted and
//!   reported, never failed), or the engine disagrees with the oracle
//!   ([`ErrorVerdict::Wrong`] — always a test failure).
//!
//! # Why one module
//!
//! The same two corpora are driven by `tests/stdlib_lists.rs` and
//! `tests/exceptions_corpus.rs` on the interpreter, by `tests/jit_real_module.rs`
//! on **both** the interpreter and the native baseline tier, and (behind the
//! `jit-cranelift` feature) by `tests/jit_cranelift_real.rs`. A per-suite copy of
//! the parser would let one suite's grammar drift from another's, so the parsing,
//! the argument/expectation builders and the comparator live here and are used by
//! every one of them.
//!
//! # The expectation grammar (exhaustive, census-derived)
//!
//! The breadth corpus's expected column uses exactly these shapes: small
//! integers (including negative ones), floats, bare atoms, `'quoted'` atoms,
//! `[]`, lists, tuples (including `{}`), byte binaries `<<1,2,3>>`, and the
//! **empty** map `#{}` (also nested inside a tuple). There are no strings, no
//! bignums, no non-empty maps, no improper lists and no non-byte bitstrings. A
//! text outside that grammar **panics**: a corpus/parser bug must never be
//! silently skipped. The *argument* grammar is the same plus `"string"` (a
//! code-point list).

use beam_common::{BeamError, ExceptionClass, Raise, Result};
use beam_engine::terms;
use beam_term::{cmp, Eterm};

use super::host::ModuleHost;

// ---------------------------------------------------------------------------
// Part 1 — the happy-path (`module:function(args) => <value>`) corpus
// ---------------------------------------------------------------------------

/// One parsed breadth-corpus line: `module:function(args) => expected`.
#[derive(Clone, Copy, Debug)]
pub struct BreadthTarget<'a> {
    /// The module to call.
    pub module: &'a str,
    /// The exported function to call.
    pub function: &'a str,
    /// The argument text, e.g. `[1,2],[3,4]` (one of the corpus shapes).
    pub args: &'a str,
    /// The verbatim `~w` rendering the system `erl` produced.
    pub expected: &'a str,
}

impl BreadthTarget<'_> {
    /// The `<module>:<function>(<args>)` label used in reports and messages.
    pub fn label(&self) -> String {
        format!("{}:{}({})", self.module, self.function, self.args)
    }
}

/// Parses one corpus line; `None` for a comment/blank line. A malformed *target*
/// line is a corpus bug and panics (`expect`), so a broken fixture can never be
/// silently skipped.
pub fn parse_breadth_target(line: &str) -> Option<BreadthTarget<'_>> {
    let line = line.split("  # oracle:").next().unwrap_or(line).trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, expected) = line
        .split_once(" => ")
        .expect("corpus line without ` => `");
    let (module, rest) = key.split_once(':').expect("corpus key without `:`");
    let (function, args) = rest.split_once('(').expect("corpus key without `(`");
    let args = args.strip_suffix(')').expect("corpus key without `)`");
    Some(BreadthTarget {
        module,
        function,
        args,
        expected,
    })
}

/// Splits an argument list on **top-level** commas, respecting `[]`/`{}`/`<<>>`
/// nesting and `"`-quoted strings.
pub fn split_arg_terms(args: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut start = 0usize;
    for (index, &byte) in args.as_bytes().iter().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b'[' | b'{' | b'<' if !in_string => depth += 1,
            b']' | b'}' | b'>' if !in_string => depth -= 1,
            b',' if !in_string && depth == 0 => {
                parts.push(&args[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts
}

/// Builds the argument list for a corpus call from its `(<args>)` text; the empty
/// text is the zero-arity call.
pub fn breadth_args(host: &mut ModuleHost, args: &str) -> Vec<Eterm> {
    if args.trim().is_empty() {
        return Vec::new();
    }
    split_arg_terms(args)
        .iter()
        .map(|part| arg_term(host, part))
        .collect()
}

/// Builds one corpus **argument** from its source text: `[]`, `[a,b,c]`,
/// `{a,b}`, `<<1,2,3>>`, `"abc"`, an integer, an atom (bare or `'quoted'`).
pub fn arg_term(host: &mut ModuleHost, text: &str) -> Eterm {
    term_from_text(host, text)
}

/// The baked oracle expectation for one breadth row.
#[derive(Clone, Copy, Debug)]
pub enum Expected {
    /// The expectation as a materialized term: an integer, float, atom, list,
    /// tuple, binary — or any of those containing a (nested) empty map.
    Term(Eterm),
    /// The **top-level** empty map `#{}`. Compared through [`terms::map_entries`]
    /// rather than through a term, so the check reads the produced map's pair list
    /// directly (an empty pair list is `#{}`, and nothing else is).
    EmptyMap,
}

impl Expected {
    /// Does `actual` — the engine's result — equal the baked oracle expectation?
    ///
    /// [`Expected::Term`] compares with [`beam_term::cmp`] against the module's
    /// atom table (the engine's own `=:=`, so boxed tuples/lists/floats compare by
    /// value); [`Expected::EmptyMap`] requires `actual` to be a map with no pairs.
    pub fn matches(&self, host: &ModuleHost, actual: Eterm) -> bool {
        match self {
            Expected::Term(expected) => cmp(actual, *expected, host.atom_table()).is_eq(),
            Expected::EmptyMap => terms::map_entries(actual).is_ok_and(|pairs| pairs.is_empty()),
        }
    }

    /// The expectation rendered in the corpus's `~w` spelling, for diagnostics.
    pub fn render(&self, host: &ModuleHost) -> Option<String> {
        match self {
            Expected::Term(term) => render_term(host, *term),
            Expected::EmptyMap => Some("#{}".to_string()),
        }
    }
}

/// Parses one breadth corpus **expectation** (the text right of ` => `) into the
/// term it names.
///
/// A text outside the corpus grammar panics: an unparsable expectation is a
/// corpus/parser bug, never a row to skip.
pub fn parse_expected_value(host: &mut ModuleHost, text: &str) -> Expected {
    if text.trim() == "#{}" {
        return Expected::EmptyMap;
    }
    Expected::Term(term_from_text(host, text))
}

/// The one recursive text → term builder behind both [`arg_term`] and
/// [`parse_expected_value`].
///
/// The shapes are exactly the corpus grammar (see the module docs). The empty map
/// is built as a zero-pair *flat* map — the frozen layout of `#{}` — because
/// [`terms::make_map`] would need the host's atom table borrowed at the same time
/// as the host itself.
fn term_from_text(host: &mut ModuleHost, text: &str) -> Eterm {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix("<<").and_then(|t| t.strip_suffix(">>")) {
        let bytes: Vec<u8> = if inner.trim().is_empty() {
            Vec::new()
        } else {
            split_arg_terms(inner)
                .iter()
                .map(|part| {
                    part.trim()
                        .parse::<u8>()
                        .expect("a corpus binary element is a byte literal")
                })
                .collect()
        };
        return terms::make_heap_binary(host, &bytes).expect("build the corpus binary argument");
    }
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        if inner.trim().is_empty() {
            return beam_term::NIL;
        }
        let elements: Vec<Eterm> = split_arg_terms(inner)
            .iter()
            .map(|part| term_from_text(host, part))
            .collect();
        return terms::make_list(host, &elements).expect("build the corpus list argument");
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        // `{}` is the EMPTY tuple: `split_arg_terms("")` yields one empty part, which
        // would otherwise become a 1-tuple holding the empty atom.
        let elements: Vec<Eterm> = if inner.trim().is_empty() {
            Vec::new()
        } else {
            split_arg_terms(inner)
                .iter()
                .map(|part| term_from_text(host, part))
                .collect()
        };
        return terms::make_tuple(host, &elements).expect("build the corpus tuple argument");
    }
    if let Some(inner) = text
        .strip_prefix('#')
        .and_then(|t| t.strip_prefix('{'))
        .and_then(|t| t.strip_suffix('}'))
    {
        assert!(
            inner.trim().is_empty(),
            "the corpus grammar has no non-empty map, got {text:?}"
        );
        return terms::make_flatmap(host, &[]).expect("build the empty map");
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        let chars: Vec<Eterm> = inner
            .bytes()
            .map(|byte| beam_term::make_small(i64::from(byte)))
            .collect();
        return terms::make_list(host, &chars).expect("build the corpus string argument");
    }
    if let Ok(value) = text.parse::<i64>() {
        return beam_term::make_small(value);
    }
    if looks_like_float(text) {
        let value: f64 = text
            .parse()
            .unwrap_or_else(|_| panic!("the corpus float {text:?} does not parse"));
        return terms::make_float(host, value).expect("build the corpus float argument");
    }
    match text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        Some(quoted) => host.intern(&unescape_atom(quoted)),
        None => host.intern(text),
    }
}

/// Is `text` a float literal rather than an atom that happens to contain `e`
/// (`eof`, `true`, …)? A float starts with a digit (or a signed digit) **and**
/// carries a `.` or an exponent.
fn looks_like_float(text: &str) -> bool {
    let bytes = text.as_bytes();
    let starts_with_digit = match bytes.first() {
        Some(byte) if byte.is_ascii_digit() => true,
        Some(b'-') | Some(b'+') => bytes
            .get(1)
            .map(|byte| byte.is_ascii_digit())
            .unwrap_or(false),
        _ => false,
    };
    starts_with_digit && text.contains(['.', 'e', 'E'])
}

/// Undoes the `'`-atom escaping the `~w` renderer applies (`\'` and `\\`).
fn unescape_atom(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The `~w` renderer (diagnostics / the corpus's own spelling)
// ---------------------------------------------------------------------------

/// The words `~w` quotes when they appear as atoms (`io_lib:write_atom/1`).
const ERESERVED: &[&str] = &[
    "after", "begin", "case", "catch", "cond", "end", "fun", "if", "let", "of", "query", "receive",
    "try", "when", "andalso", "orelse", "bsl", "bsr", "band", "bnot", "bor", "bxor", "div", "rem",
    "and", "or", "not", "xor", "maybe", "else",
];

/// Renders an atom exactly as `~w` does: bare when it is a plain lowercase name
/// that is not a reserved word, `'quoted'` otherwise.
pub fn render_atom(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .map(|first| first.is_ascii_lowercase())
        .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')
        && !ERESERVED.contains(&name);
    if plain {
        return name.to_string();
    }
    let mut out = String::from("'");
    for c in name.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Renders a float the way `~w` does (`1.0` rather than `1`).
pub fn render_float(value: f64) -> String {
    let text = format!("{value}");
    if text.contains(['.', 'e', 'E']) {
        text
    } else {
        format!("{text}.0")
    }
}

/// Renders a bitstring the way `~w` does — always as byte values, never as a
/// `<<"…">>` string (`~w` does no string detection).
pub fn render_binary(bytes: &[u8]) -> String {
    let mut out = String::from("<<");
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&byte.to_string());
    }
    out.push_str(">>");
    out
}

/// The engine's `~w`-equivalent term renderer — the form the corpus's expected
/// values are written in. `None` for a term `~w` cannot render *deterministically*
/// (a fun, pid, port or reference); such a result is reported as a loud skip
/// rather than compared against a fabricated string.
pub fn render_term(host: &ModuleHost, term: Eterm) -> Option<String> {
    if beam_term::is_small(term) {
        return Some(beam_term::signed_value(term).to_string());
    }
    if beam_term::is_atom(term) {
        return host
            .atom_table()
            .name(beam_term::atom_val(term))
            .map(render_atom);
    }
    if beam_term::is_nil(term) {
        return Some("[]".to_string());
    }
    if let Ok(value) = terms::float_value(term) {
        return Some(render_float(value));
    }
    if let Ok(bytes) = terms::binary_bytes(term) {
        return Some(render_binary(bytes));
    }
    if let Ok(arity) = terms::tuple_arity(term) {
        let mut out = String::from("{");
        for index in 0..arity {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render_term(host, terms::tuple_element(term, index).ok()?)?);
        }
        out.push('}');
        return Some(out);
    }
    if let Ok(pairs) = terms::map_entries(term) {
        let mut out = String::from("#{");
        for (index, &(key, value)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render_term(host, key)?);
            out.push_str(" => ");
            out.push_str(&render_term(host, value)?);
        }
        out.push('}');
        return Some(out);
    }
    if beam_term::is_list(term) {
        let elements = terms::list_elements(term).ok()?;
        let mut out = String::from("[");
        for (index, &element) in elements.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render_term(host, element)?);
        }
        out.push(']');
        return Some(out);
    }
    None
}

// ---------------------------------------------------------------------------
// Part 2 — the error-path (`module:function/arity <poisons> => <reason>`) corpus
// ---------------------------------------------------------------------------

/// The atom every `atom`-shaped poison stands for (it matches no clause).
pub const POISON_ATOM: &str = "genesis_poison";

/// The poison argument a target function is called with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Poison {
    /// The atom [`POISON_ATOM`].
    Atom,
    /// The small integer `7`.
    Int,
    /// The empty list `[]`.
    Nil,
    /// The empty tuple `{}`.
    Tuple0,
}

/// Parses one poison token, or `None` for an unknown token.
pub fn parse_poison(token: &str) -> Option<Poison> {
    match token {
        "atom" => Some(Poison::Atom),
        "int" => Some(Poison::Int),
        "nil" => Some(Poison::Nil),
        "tuple0" => Some(Poison::Tuple0),
        _ => None,
    }
}

/// Builds one poison argument on `host`'s arena (which outlives the run).
pub fn poison_term(host: &mut ModuleHost, poison: Poison) -> Eterm {
    match poison {
        Poison::Atom => host.intern(POISON_ATOM),
        Poison::Int => beam_term::make_small(7),
        Poison::Nil => beam_term::NIL,
        Poison::Tuple0 => terms::make_tuple(host, &[]).expect("build the empty tuple"),
    }
}

/// The exception a poisoned target must produce.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExpectedError {
    /// A bare-atom reason with this name (`function_clause`).
    Atom(String),
    /// The 2-tuple reason `{tag, <poison>}` with this tag (`case_clause`/
    /// `badmatch`/`badrecord`).
    Tuple(String),
    /// The engine must instead return a typed error (a documented host gap).
    Unsupported,
}

/// One parsed error-path corpus line.
#[derive(Clone, Debug)]
pub struct ErrorTarget {
    /// The module to call.
    pub module: String,
    /// The exported function to call.
    pub function: String,
    /// The arity (equal to `poisons.len()`).
    pub arity: u32,
    /// One poison per argument (the file's `POISONS` list; its length is the arity).
    pub poisons: Vec<Poison>,
    /// The exception the system `erl` raises for the call.
    pub expected: ExpectedError,
    /// The quoted `erl -noshell -eval …` command backing the expectation (empty for
    /// an `unsupported` line, whose expectation is the engine's own gap).
    pub oracle: String,
}

impl ErrorTarget {
    /// The `<module>:<function>/<arity>` label used in the report.
    pub fn label(&self) -> String {
        format!("{}:{}/{}", self.module, self.function, self.arity)
    }

    /// The poison argument terms, built on `host`'s arena (which outlives the run).
    pub fn args(&self, host: &mut ModuleHost) -> Vec<Eterm> {
        self.poisons
            .iter()
            .map(|poison| poison_term(host, *poison))
            .collect()
    }

    /// The reason term `expected` names — a bare atom, or the 2-tuple
    /// `{tag, payload}` whose payload is the LAST poison the target was called
    /// with (the file's documented rule).
    pub fn reason_term(&self, host: &mut ModuleHost) -> Option<Eterm> {
        match &self.expected {
            ExpectedError::Atom(name) => Some(host.intern(name)),
            ExpectedError::Tuple(tag) => {
                let payload = poison_term(
                    host,
                    *self
                        .poisons
                        .last()
                        .expect("every error-path target has at least one poison"),
                );
                let tag = host.intern(tag);
                Some(terms::make_tuple(host, &[tag, payload]).expect("build the {tag, arg} reason"))
            }
            ExpectedError::Unsupported => None,
        }
    }
}

/// Parses the committed error-path corpus, panicking (a test-authoring bug, never
/// runtime data) on a malformed line.
pub fn parse_error_targets(text: &str) -> Vec<ErrorTarget> {
    let mut targets = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The assertion's justification rides in a trailing `# …` comment.
        let (spec, oracle) = match line.split_once('#') {
            Some((spec, comment)) => (spec.trim(), comment.trim().to_string()),
            None => (line, String::new()),
        };
        let tokens: Vec<&str> = spec.split_whitespace().collect();
        assert_eq!(
            tokens.len(),
            4,
            "line {}: expected `<module>:<f>/<n> <poison> => <expected>`, got {spec:?}",
            index + 1
        );
        assert_eq!(
            tokens[2],
            "=>",
            "line {}: missing the `=>` separator: {spec:?}",
            index + 1
        );
        let (module, rest) = tokens[0]
            .split_once(':')
            .unwrap_or_else(|| panic!("line {}: no `:` in {:?}", index + 1, tokens[0]));
        let (function, arity) = rest
            .split_once('/')
            .unwrap_or_else(|| panic!("line {}: no `/` in {:?}", index + 1, tokens[0]));
        let arity: u32 = arity
            .parse()
            .unwrap_or_else(|_| panic!("line {}: bad arity in {:?}", index + 1, tokens[0]));
        let poisons: Vec<Poison> = tokens[1]
            .split(',')
            .map(|token| {
                parse_poison(token)
                    .unwrap_or_else(|| panic!("line {}: unknown poison {:?}", index + 1, token))
            })
            .collect();
        assert_eq!(
            poisons.len(),
            arity as usize,
            "line {}: {} poison(s) for arity {arity}: {:?}",
            index + 1,
            poisons.len(),
            tokens[1]
        );
        targets.push(ErrorTarget {
            module: module.to_string(),
            function: function.to_string(),
            arity,
            poisons,
            expected: parse_expected_error(tokens[3]),
            oracle,
        });
    }
    targets
}

/// Parses one expected-reason token (`unsupported`, a bare atom name, or `{tag}`).
fn parse_expected_error(token: &str) -> ExpectedError {
    if token == "unsupported" {
        return ExpectedError::Unsupported;
    }
    if let Some(inner) = token.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        return ExpectedError::Tuple(inner.to_string());
    }
    ExpectedError::Atom(token.to_string())
}

/// Renders a [`BeamError`] compactly for a `BLOCKED:` line: the engine's typed gap
/// is usually a `Raise` whose reason is the `unsupported_opcode: <what>` atom, so
/// print that atom; anything else prints its `Debug`.
pub fn describe_error(error: &BeamError) -> String {
    if let BeamError::Raise(raise) = error {
        if let Some(reason) = raise.reason.as_atom() {
            return format!("typed raise: {reason}");
        }
    }
    format!("{error:?}")
}

/// How one tier's outcome of an error-path target related to the oracle.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ErrorVerdict {
    /// The engine reproduced the oracle's exception class **and** reason term.
    Matches,
    /// The engine answered a typed gap instead of executing the compiled raise (a
    /// documented coverage gap — counted and reported, never a failure).
    TypedGap(String),
    /// The engine disagrees with the oracle — always a test failure.
    Wrong(String),
}

/// The live reason term a structured raise carries out of the machine.
fn live_reason(raise: &Raise) -> Option<Eterm> {
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}

/// Verifies one tier's outcome of `target` against the oracle's expectation.
///
/// The three arms mirror the corpus's own policy:
/// * [`ExpectedError::Unsupported`] — a typed `Err` is the documented gap
///   ([`ErrorVerdict::TypedGap`]); a value would be fabricated
///   ([`ErrorVerdict::Wrong`]).
/// * [`ExpectedError::Atom`] / [`ExpectedError::Tuple`] — a raise of class `error`
///   whose reason is the expected atom (or the expected `{tag, last poison}` tuple)
///   is [`ErrorVerdict::Matches`]; a raise whose reason is the engine's
///   `unsupported_opcode: <what>` gap marker, a raise carrying no live reason term
///   for a structured expectation, or a non-raise typed error is
///   [`ErrorVerdict::TypedGap`]; a value, a different class or a different reason is
///   [`ErrorVerdict::Wrong`].
pub fn verify_error(
    host: &mut ModuleHost,
    outcome: &Result<Eterm>,
    target: &ErrorTarget,
) -> ErrorVerdict {
    match &target.expected {
        ExpectedError::Unsupported => match outcome {
            Ok(value) => ErrorVerdict::Wrong(format!(
                "{} returned {value:?}; the oracle's outcome is a typed engine gap \
                 (`unsupported`), so a value is fabricated",
                target.label()
            )),
            Err(error) => ErrorVerdict::TypedGap(describe_error(error)),
        },
        expected => match outcome {
            Err(BeamError::Raise(raise)) => {
                if raise.class != ExceptionClass::Error {
                    return ErrorVerdict::Wrong(format!(
                        "{} raised class `{}`, the oracle's class is `error`  [{}]",
                        target.label(),
                        raise.class.as_str(),
                        target.oracle
                    ));
                }
                // A bare-atom reason names the atom directly; otherwise (or for the
                // tuple shape) the raise's live reason TERM must equal the oracle's.
                if let ExpectedError::Atom(name) = expected {
                    if let Some(reason_atom) = raise.reason.as_atom() {
                        if reason_atom == name {
                            return ErrorVerdict::Matches;
                        }
                        // The engine's typed-gap marker: the compiled raise never ran
                        // (an `unsupported_opcode: <what>` diagnostic atom).
                        if reason_atom.starts_with("unsupported_opcode") {
                            return ErrorVerdict::TypedGap(describe_error(&BeamError::Raise(
                                raise.clone(),
                            )));
                        }
                        return ErrorVerdict::Wrong(format!(
                            "{} raised the atom `{reason_atom}`, the oracle raises `{name}`  [{}]",
                            target.label(),
                            target.oracle
                        ));
                    }
                }
                let Some(reason) = live_reason(raise) else {
                    return ErrorVerdict::TypedGap(describe_error(&BeamError::Raise(
                        raise.clone(),
                    )));
                };
                let want = target
                    .reason_term(host)
                    .expect("a non-`unsupported` expectation names a reason term");
                if cmp(reason, want, host.atom_table()).is_eq() {
                    ErrorVerdict::Matches
                } else {
                    ErrorVerdict::Wrong(format!(
                        "{} raised a DIFFERENT reason than the oracle: got {reason:?}, \
                         want {want:?}  [{}]",
                        target.label(),
                        target.oracle
                    ))
                }
            }
            // A typed error without a raise: a coverage gap (reported, not failed).
            Err(error) => ErrorVerdict::TypedGap(describe_error(error)),
            // A value where the oracle raises: a fabricated result — a failure.
            Ok(value) => ErrorVerdict::Wrong(format!(
                "{} returned {value:?}; the oracle raises  [{}]",
                target.label(),
                target.oracle
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use beam_common::{ExitReason, LiveReason};
    use beam_term::NIL;

    use super::*;

    /// A host with no OTP install behind it: the corpus builders only need the
    /// shared arena and the shared atom table.
    fn host() -> ModuleHost {
        ModuleHost::new(PathBuf::from("/nonexistent-otp-install"))
    }

    /// Renders a parsed expectation back into the corpus's own spelling.
    fn round_trip(text: &str) -> String {
        let mut host = host();
        let expected = parse_expected_value(&mut host, text);
        expected
            .render(&host)
            .unwrap_or_else(|| panic!("{text:?} rendered to nothing"))
    }

    #[test]
    fn the_expected_value_grammar_round_trips_through_terms() {
        for text in [
            "0",
            "39",
            "-1",
            "3.141592653589793",
            "6.283185307179586",
            "[]",
            "ok",
            "true",
            "eof",
            "'maybe'",
            "'$prepared$'",
            "'div'",
            "{}",
            "{ok,1}",
            "{error,{invalid_restart_type,2}}",
            "[a,b]",
            "[1,2,3]",
            "[a,p,'div',blockquote]",
            "[{},[]]",
            "<<1,2,3>>",
            "<<>>",
            "{value,<<>>,[]}",
            "{l,<<97,98>>}",
            "#{}",
        ] {
            assert_eq!(round_trip(text), text, "expectation {text:?} changed shape");
        }
    }

    #[test]
    fn a_negative_integer_is_a_negative_small_and_a_float_is_not_an_atom() {
        let mut host = host();
        let Expected::Term(negative) = parse_expected_value(&mut host, "-1") else {
            panic!("`-1` must be a term");
        };
        assert!(beam_term::is_small(negative));
        assert_eq!(beam_term::signed_value(negative), -1);

        let Expected::Term(float) = parse_expected_value(&mut host, "3.141592653589793") else {
            panic!("a float must be a term");
        };
        let value = terms::float_value(float).expect("the parsed term is a float");
        assert!((value - std::f64::consts::PI).abs() < f64::EPSILON);

        // The float detector must not swallow an atom that merely contains `e`.
        let Expected::Term(atom) = parse_expected_value(&mut host, "eof") else {
            panic!("`eof` must be a term");
        };
        assert!(beam_term::is_atom(atom));
        assert_eq!(
            host.atom_table()
                .name(beam_term::atom_val(atom))
                .expect("the atom names itself"),
            "eof"
        );
    }

    #[test]
    fn a_quoted_atom_is_unescaped_before_it_is_interned() {
        let mut host = host();
        let Expected::Term(atom) = parse_expected_value(&mut host, "'a\\'b'") else {
            panic!("a quoted atom must be a term");
        };
        assert_eq!(
            host.atom_table()
                .name(beam_term::atom_val(atom))
                .expect("the atom names itself"),
            "a'b"
        );
    }

    #[test]
    fn an_empty_map_expectation_matches_only_a_map_with_no_pairs() {
        let mut host = host();
        let expected = parse_expected_value(&mut host, "#{}");
        assert!(matches!(expected, Expected::EmptyMap));
        let empty = terms::make_flatmap(&mut host, &[]).expect("build the empty map");
        assert!(expected.matches(&host, empty), "`#{{}}` must match `#{{}}`");

        let key = host.intern("k");
        let value = beam_term::make_small(1);
        let one_pair = terms::make_flatmap(&mut host, &[(key, value)]).expect("build a 1-pair map");
        assert!(
            !expected.matches(&host, one_pair),
            "`#{{}}` must not match a non-empty map"
        );
        assert!(
            !expected.matches(&host, NIL),
            "`#{{}}` must not match a non-map"
        );
    }

    #[test]
    fn a_nested_empty_map_expectation_holds_its_shape() {
        let mut host = host();
        let expected = parse_expected_value(&mut host, "{asm,#{},[],{0,nil}}");
        assert_eq!(round_trip("{asm,#{},[],{0,nil}}"), "{asm,#{},[],{0,nil}}");
        // And it is a real term of the right shape.
        let Expected::Term(term) = expected else {
            panic!("a tuple is a term");
        };
        assert_eq!(terms::tuple_arity(term).expect("a tuple"), 4);
    }

    #[test]
    fn a_value_expectation_matches_the_same_term_and_rejects_another() {
        let mut host = host();
        let expected = parse_expected_value(&mut host, "{ok,1}");
        let ok = host.intern("ok");
        let same = terms::make_tuple(&mut host, &[ok, beam_term::make_small(1)])
            .expect("build {ok,1}");
        assert!(expected.matches(&host, same));
        let ok = host.intern("ok");
        let different = terms::make_tuple(&mut host, &[ok, beam_term::make_small(2)])
            .expect("build {ok,2}");
        assert!(!expected.matches(&host, different));
    }

    #[test]
    #[should_panic(expected = "no non-empty map")]
    fn a_non_empty_map_expectation_is_a_corpus_bug() {
        let mut host = host();
        let _ = parse_expected_value(&mut host, "#{k => 1}");
    }

    #[test]
    fn the_breadth_parser_reads_every_field_and_skips_comments() {
        let line = "array:is_array(0) => false  # oracle: erl -noshell -eval '…'";
        let target = parse_breadth_target(line).expect("a target line");
        assert_eq!(target.module, "array");
        assert_eq!(target.function, "is_array");
        assert_eq!(target.args, "0");
        assert_eq!(target.expected, "false");
        assert_eq!(target.label(), "array:is_array(0)");
        assert!(parse_breadth_target("# a comment").is_none());
        assert!(parse_breadth_target("   ").is_none());
    }

    #[test]
    fn breadth_args_splits_only_on_top_level_commas() {
        let mut host = host();
        let args = breadth_args(&mut host, "[1,2],{a,b},<<3,4>>");
        assert_eq!(args.len(), 3, "three top-level arguments");
        assert_eq!(terms::list_elements(args[0]).expect("a list").len(), 2);
        assert_eq!(terms::tuple_arity(args[1]).expect("a tuple"), 2);
        assert_eq!(terms::binary_bytes(args[2]).expect("a binary"), &[3, 4]);
        assert!(breadth_args(&mut host, "  ").is_empty());
    }

    #[test]
    fn the_error_corpus_parser_reads_poisons_expectation_and_oracle() {
        let text = "\
# a comment
base64:decode/1 atom => function_clause  # oracle: erl -noshell -eval 'try base64:decode(genesis_poison) catch C:R -> io:format(\"~p:~p~n\",[C,R]) end,halt().'
disk_log:ll_close/1 int => {case_clause}  # oracle: erl -noshell -eval '…'
beam_asm:encode/2 atom,atom => unsupported
";
        let targets = parse_error_targets(text);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].label(), "base64:decode/1");
        assert_eq!(targets[0].expected, ExpectedError::Atom("function_clause".into()));
        assert!(targets[0].oracle.starts_with("oracle: erl -noshell"));
        assert_eq!(targets[1].poisons, vec![Poison::Int]);
        assert_eq!(targets[1].expected, ExpectedError::Tuple("case_clause".into()));
        assert_eq!(targets[2].poisons, vec![Poison::Atom, Poison::Atom]);
        assert_eq!(targets[2].arity, 2);
        assert_eq!(targets[2].expected, ExpectedError::Unsupported);
    }

    #[test]
    fn a_matching_atom_reason_is_matches_and_a_different_one_is_wrong() {
        let mut host = host();
        let target = ErrorTarget {
            module: "base64".into(),
            function: "decode".into(),
            arity: 1,
            poisons: vec![Poison::Atom],
            expected: ExpectedError::Atom("function_clause".into()),
            oracle: "oracle: erl -noshell -eval '…'".into(),
        };

        let matching = BeamError::Raise(Raise::error(ExitReason::atom("function_clause")));
        assert_eq!(
            verify_error(&mut host, &Err(matching), &target),
            ErrorVerdict::Matches
        );

        // The same shape carried as a live reason TERM (the engine's structured
        // raise) must also match.
        let reason = host.intern("function_clause");
        let raised = BeamError::Raise(
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(reason)),
        );
        assert_eq!(
            verify_error(&mut host, &Err(raised), &target),
            ErrorVerdict::Matches
        );

        let wrong_reason = BeamError::Raise(Raise::error(ExitReason::atom("badarg")));
        assert!(matches!(
            verify_error(&mut host, &Err(wrong_reason), &target),
            ErrorVerdict::Wrong(_)
        ));

        let wrong_class = BeamError::Raise(Raise::new(
            ExceptionClass::Throw,
            ExitReason::atom("function_clause"),
        ));
        assert!(matches!(
            verify_error(&mut host, &Err(wrong_class), &target),
            ErrorVerdict::Wrong(_)
        ));

        // A value where the oracle raises is a fabricated result.
        assert!(matches!(
            verify_error(&mut host, &Ok(NIL), &target),
            ErrorVerdict::Wrong(_)
        ));
    }

    #[test]
    fn an_unexecuted_raise_is_a_typed_gap_not_a_wrong_result() {
        let mut host = host();
        let target = ErrorTarget {
            module: "base64".into(),
            function: "decode".into(),
            arity: 1,
            poisons: vec![Poison::Atom],
            expected: ExpectedError::Atom("function_clause".into()),
            oracle: String::new(),
        };
        // The `unsupported_opcode` gap: class `error`, a diagnostic atom reason and
        // no live reason term — the compiled raise never ran.
        let gap = BeamError::Raise(Raise::error(ExitReason::atom(
            "unsupported_opcode: test: gap",
        )));
        assert!(matches!(
            verify_error(&mut host, &Err(gap), &target),
            ErrorVerdict::TypedGap(_)
        ));
        // A non-raise typed error is the same kind of gap.
        assert!(matches!(
            verify_error(&mut host, &Err(BeamError::BadArg), &target),
            ErrorVerdict::TypedGap(_)
        ));

        // An `unsupported` row: a typed error is the expected gap, a value is wrong.
        let unsupported = ErrorTarget {
            expected: ExpectedError::Unsupported,
            ..target
        };
        assert!(matches!(
            verify_error(&mut host, &Err(BeamError::BadArg), &unsupported),
            ErrorVerdict::TypedGap(_)
        ));
        assert!(matches!(
            verify_error(&mut host, &Ok(NIL), &unsupported),
            ErrorVerdict::Wrong(_)
        ));
    }

    #[test]
    fn a_tuple_reason_carries_the_last_poison_as_its_payload() {
        let mut host = host();
        let target = ErrorTarget {
            module: "disk_log".into(),
            function: "ll_close".into(),
            arity: 2,
            poisons: vec![Poison::Int, Poison::Atom],
            expected: ExpectedError::Tuple("case_clause".into()),
            oracle: String::new(),
        };
        // `{case_clause, genesis_poison}` — the payload is the LAST poison.
        let payload = host.intern(POISON_ATOM);
        let tag = host.intern("case_clause");
        let reason = terms::make_tuple(&mut host, &[tag, payload]).expect("build the reason");
        let raised = BeamError::Raise(
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(reason)),
        );
        assert_eq!(
            verify_error(&mut host, &Err(raised), &target),
            ErrorVerdict::Matches
        );

        // A bare atom reason cannot satisfy a tuple expectation.
        let bare = BeamError::Raise(Raise::error(ExitReason::atom("case_clause")));
        assert!(matches!(
            verify_error(&mut host, &Err(bare), &target),
            ErrorVerdict::TypedGap(_)
        ));
    }
}
