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

//! Pattern text → AST, plus the exact `{Msg, Pos}` compile errors.
//!
//! A recursive-descent parser over the pattern's **codepoints** (bytes in
//! non-`unicode` mode, UTF-8 scalars in `unicode` mode) whose error positions
//! are **byte offsets into the raw pattern**, which is what the oracle reports
//! (its `erroffset` is a `PCRE2_SIZE` into the byte buffer PCRE2 was handed).
//!
//! # Error fidelity
//!
//! Every message string is copied from the bundled PCRE2 10.4x table
//! (`../../../erts/emulator/pcre/pcre2_error.c`), never invented. Error
//! *positions* are reproduced for the constructs this engine supports; the
//! parser has no "roughly here" mode — a construct it cannot parse exactly
//! raises the crate's typed `unsupported_bif` instead of fabricating a
//! `{Msg, Pos}`.
//!
//! # Supported surface
//!
//! Literals · `.` · character classes (ranges, negation, `\d\D\w\W\s\S\h\H\v\V`,
//! POSIX `[[:name:]]`, `\Q…\E`) · anchors `^ $ \A \z \Z \b \B \G` · alternation ·
//! capturing / non-capturing / named groups · atomic groups `(?>…)` · inline and
//! scoped option groups `(?i)`, `(?i:…)`, `(?#…)` · quantifiers `* + ? {n} {n,}
//! {n,m}` in greedy, lazy and possessive form · backreferences (`\1`, `\g{…}`,
//! `\k<name>`) · the escapes `\n \r \t \f \a \e \0 \xHH \x{…} \o{…} \cX \ddd`.
//!
//! Everything else — lookarounds, `(?|`, conditionals, recursion, `\K`, `\R`,
//! `\X`, `\C`, `\p{…}`, `(?(DEFINE)…)`, callouts, `(*VERB)` — is the typed
//! `unsupported_bif` gap (see `mod.rs` for the per-construct rationale).
//!
//! # Case folding
//!
//! [`fold_key`] is a simple-fold canonicaliser covering ASCII, Latin-1
//! Supplement, Latin Extended-A, Greek and Cyrillic — the ranges the oracle's
//! `caseless` behaviour is exercised over here. A codepoint outside them folds
//! to itself, so `caseless` over an exotic script compares exactly; that is a
//! documented coverage limit, not a silently wrong match for the covered set.
//!
//! # Why this file is long
//!
//! The parser is one coherent state machine over the pattern's codepoints, and
//! the `E_*` table above it is one list of PCRE2 diagnostics whose messages and
//! byte positions are pinned exactly; splitting either would spread a single
//! diagnostic table and a single cursor discipline across files. The length is
//! that parser plus its `#[cfg(test)]` suite, not accumulated special cases.

use crate::error::unsupported_bif;
use crate::re::options::CompileFlags;

// ---------------------------------------------------------------------------
// Compile-error messages (verbatim from `pcre2_error.c`)
// ---------------------------------------------------------------------------

pub(crate) const E_BS_AT_END: &str = "\\ at end of pattern";
pub(crate) const E_BS_C_AT_END: &str = "\\c at end of pattern";
pub(crate) const E_UNRECOGNIZED_ESCAPE: &str = "unrecognized character follows \\";
pub(crate) const E_NUMBERS_OUT_OF_ORDER: &str = "numbers out of order in {} quantifier";
pub(crate) const E_NUMBER_TOO_BIG: &str = "number too big in {} quantifier";
pub(crate) const E_MISSING_BRACKET: &str = "missing terminating ] for character class";
pub(crate) const E_ESCAPE_IN_CLASS: &str = "escape sequence is invalid in character class";
pub(crate) const E_RANGE_OUT_OF_ORDER: &str = "range out of order in character class";
pub(crate) const E_NOTHING_TO_REPEAT: &str = "quantifier does not follow a repeatable item";
pub(crate) const E_BAD_GROUP_OPTION: &str = "unrecognized character after (? or (?-";
pub(crate) const E_MISSING_PAREN: &str = "missing closing parenthesis";
pub(crate) const E_BAD_REFERENCE: &str = "reference to non-existent subpattern";
pub(crate) const E_BAD_COMMENT: &str = "missing ) after (?# comment";
pub(crate) const E_TOO_DEEP: &str = "parentheses are too deeply nested";
pub(crate) const E_UNMATCHED_PAREN: &str = "unmatched closing parenthesis";
pub(crate) const E_CODEPOINT_TOO_LARGE: &str =
    "character code point value in \\x{} or \\o{} is too large";
pub(crate) const E_UNKNOWN_POSIX: &str = "unknown POSIX class name";
pub(crate) const E_NAME_DUP: &str =
    "two named subpatterns have the same name (PCRE2_DUPNAMES not set)";
pub(crate) const E_NAME_DIGIT: &str = "subpattern name must start with a non-digit";
pub(crate) const E_NAME_EXPECTED: &str = "subpattern name expected";
pub(crate) const E_DIGITS_MISSING: &str = "digits missing after \\x or in \\x{} or \\o{} or \\N{U+}";
pub(crate) const E_HEX_NON_HEX: &str = "non-hex character in \\x{} (closing brace missing?)";
pub(crate) const E_OCTAL_TOO_BIG: &str = "octal value is greater than \\377 in 8-bit non-UTF-8 mode";
pub(crate) const E_UTF_DISABLED: &str = "using UTF is disabled by the application";
/// The PCRE2 diagnostic for a POSIX class used outside `[...]`. Kept with the
/// other `E_*` message strings: this parser recognises `[[:name:]]` only inside
/// a class, so nothing raises this one today.
#[allow(dead_code)] // no caller: `[[:name:]]` is only parsed inside a class
pub(crate) const E_POSIX_OUTSIDE: &str = "POSIX named classes are supported only within a class";

/// A compile failure: the PCRE2 message text plus the byte offset into the
/// pattern (`{error, {Msg, Pos}}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    /// The PCRE2 error string.
    pub msg: &'static str,
    /// The 0-based byte offset into the pattern.
    pub pos: usize,
}

impl ParseError {
    fn new(msg: &'static str, pos: usize) -> ParseError {
        ParseError { msg, pos }
    }
}

// ---------------------------------------------------------------------------
// Case folding
// ---------------------------------------------------------------------------

/// The simple-fold canonical form of `c` (see the module docs for coverage).
pub(crate) fn fold_key(c: u32) -> u32 {
    match c {
        0x41..=0x5A => c + 32,
        0xC0..=0xDE if c != 0xD7 => c + 32,
        0x100..=0x137 if c % 2 == 0 => c + 1,
        0x139..=0x147 if c % 2 == 1 => c + 1,
        0x14A..=0x176 if c % 2 == 0 => c + 1,
        0x179..=0x17D if c % 2 == 1 => c + 1,
        // The (U+00FF `ÿ`, U+0178 `Ÿ`) case pair canonicalises to U+0178, so both
        // members fold to the same key and compare equal under `caseless`.
        0xFF => 0x178,
        0xB5 => 0x3BC,
        0x386 => 0x3AC,
        0x388..=0x38A => c + 37,
        0x38C => 0x3CC,
        0x38E..=0x38F => c + 63,
        0x391..=0x3A1 => c + 32,
        0x3A3..=0x3AB => c + 32,
        0x3C2 => 0x3C3,
        0x400..=0x40F => c + 80,
        0x410..=0x42F => c + 32,
        0x460..=0x480 if c % 2 == 0 => c + 1,
        0x48A..=0x4BE if c % 2 == 0 => c + 1,
        0x4C0 => 0x4CF,
        0x4C1..=0x4CD if c % 2 == 1 => c + 1,
        0x4D0..=0x52E if c % 2 == 0 => c + 1,
        _ => c,
    }
}

/// Calls `f(lower, upper)` for every case pair [`fold_key`] knows.
///
/// Used to fold a character class under `caseless` without materialising a huge
/// static table.
fn for_each_fold_pair(mut f: impl FnMut(u32, u32)) {
    for c in 0x41..=0x5A {
        f(c, c + 32);
    }
    for c in 0xC0..=0xDE {
        if c != 0xD7 {
            f(c, c + 32);
        }
    }
    f(0xB5, 0x3BC);
    f(0xFF, 0x178);
    for c in (0x100..=0x136).step_by(2) {
        f(c, c + 1);
    }
    for c in (0x139..=0x147).step_by(2) {
        f(c, c + 1);
    }
    for c in (0x14A..=0x176).step_by(2) {
        f(c, c + 1);
    }
    for c in (0x179..=0x17D).step_by(2) {
        f(c, c + 1);
    }
    f(0x386, 0x3AC);
    f(0x388, 0x3AD);
    f(0x389, 0x3AE);
    f(0x38A, 0x3AF);
    f(0x38C, 0x3CC);
    f(0x38E, 0x3CD);
    f(0x38F, 0x3CE);
    for c in 0x391..=0x3A1 {
        f(c, c + 32);
    }
    for c in 0x3A3..=0x3AB {
        f(c, c + 32);
    }
    f(0x3C2, 0x3C3);
    for c in 0x400..=0x40F {
        f(c, c + 80);
    }
    for c in 0x410..=0x42F {
        f(c, c + 32);
    }
    for c in (0x460..=0x480).step_by(2) {
        f(c, c + 1);
    }
    for c in (0x48A..=0x4BE).step_by(2) {
        f(c, c + 1);
    }
    f(0x4C0, 0x4CF);
    for c in (0x4C1..=0x4CD).step_by(2) {
        f(c, c + 1);
    }
    for c in (0x4D0..=0x52E).step_by(2) {
        f(c, c + 1);
    }
}

/// Whether `a` and `b` are the same subject character under `caseless`.
pub(crate) fn chars_equal(a: u32, b: u32, caseless: bool) -> bool {
    a == b || (caseless && fold_key(a) == fold_key(b))
}

// ---------------------------------------------------------------------------
// Character sets
// ---------------------------------------------------------------------------

/// A character set as sorted, disjoint, inclusive codepoint ranges.
pub(crate) type Ranges = Vec<(u32, u32)>;

/// `c ∈ ranges` (binary search over the sorted ranges).
pub(crate) fn ranges_contain(ranges: &Ranges, c: u32) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Adds `lo..=hi` to `ranges` and re-normalises (sorted, coalesced).
pub(crate) fn ranges_add(ranges: &mut Ranges, lo: u32, hi: u32) {
    ranges.push((lo, hi));
    ranges_normalize(ranges);
}

/// Sorts and coalesces `ranges` in place.
pub(crate) fn ranges_normalize(ranges: &mut Ranges) {
    ranges.sort_unstable();
    let mut out: Ranges = Vec::with_capacity(ranges.len());
    for &(lo, hi) in ranges.iter() {
        match out.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) && lo >= last.0 => {
                if hi > last.1 {
                    last.1 = hi;
                }
            }
            _ => out.push((lo, hi)),
        }
    }
    *ranges = out;
}

/// Complements `ranges` inside `0..=max` (PCRE2's negation, which is bounded by
/// the code-unit width for a non-`unicode` subject).
pub(crate) fn ranges_negate(ranges: &mut Ranges, max: u32) {
    ranges_normalize(ranges);
    let mut out: Ranges = Vec::with_capacity(ranges.len() + 1);
    let mut next = 0u32;
    for &(lo, hi) in ranges.iter() {
        if lo > next {
            out.push((next, lo - 1));
        }
        next = hi.saturating_add(1);
        if next > max {
            break;
        }
    }
    if next <= max {
        out.push((next, max));
    }
    *ranges = out;
}

/// Adds the case-folded counterpart of every member of `ranges`.
pub(crate) fn ranges_add_folds(ranges: &mut Ranges) {
    for_each_fold_pair(|lo, up| {
        if ranges_contain(ranges, lo) && !ranges_contain(ranges, up) {
            ranges_add(ranges, up, up);
        }
        if ranges_contain(ranges, up) && !ranges_contain(ranges, lo) {
            ranges_add(ranges, lo, lo);
        }
    });
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// A zero-width assertion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Assert {
    /// `^` — subject start, or a line start under the given `multiline`.
    StartLine { multiline: bool },
    /// `$` — subject end (or before a final newline), or a line end.
    EndLine {
        multiline: bool,
        dollar_endonly: bool,
    },
    /// `\A`.
    StartSubject,
    /// `\z`.
    EndSubject,
    /// `\Z`.
    EndSubjectOpt,
    /// `\G` — the position the match attempt started at.
    StartMatch,
    /// `\b` (`true`) or `\B` (`false`).
    WordBoundary(bool),
}

/// The parsed pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ast {
    /// An empty sequence.
    Empty,
    /// A literal codepoint plus its effective `caseless` flag.
    Char(u32, bool),
    /// `.` plus its effective `dotall` flag.
    AnyChar(bool),
    /// `\N` — any character except a newline of the compiled convention.
    AnyButNewline,
    /// A character class, already case-folded and negated.
    Class(Ranges),
    /// A sequence.
    Concat(Vec<Ast>),
    /// An alternation.
    Alt(Vec<Ast>),
    /// A quantifier.
    Repeat {
        /// The repeated item.
        item: Box<Ast>,
        /// The minimum repetition count.
        min: u32,
        /// The maximum repetition count (`None` = unbounded).
        max: Option<u32>,
        /// Greedy (`true`) or lazy (`false`).
        greedy: bool,
        /// Whether the quantifier is possessive (`*+`).
        possessive: bool,
    },
    /// A capturing group with the given (1-based) index.
    Capture { index: u32, item: Box<Ast> },
    /// A backreference to the given (1-based) group, carrying the effective
    /// `caseless` state of the enclosing inline options.
    BackRef {
        /// The referenced group (1-based).
        group: u32,
        /// Whether the reference matches case-insensitively.
        caseless: bool,
    },
    /// A zero-width assertion.
    Assert(Assert),
    /// An atomic group `(?>…)`.
    Atomic(Box<Ast>),
}

/// What [`parse`] produces.
#[derive(Clone, Debug)]
pub(crate) struct Syntax {
    /// The pattern.
    pub ast: Ast,
    /// The number of capturing groups.
    pub ncap: u32,
    /// `(name, group)` pairs, in the oracle's name-table order (by name, then
    /// by group for duplicates).
    pub names: Vec<(String, u32)>,
    /// Whether any class or literal in the pattern explicitly matches `\r` or
    /// `\n` — PCRE2's `PCRE2_HASCRORLF`, which suppresses the CRLF bump-along
    /// skip in `pcre2_match`.
    pub has_cr_or_lf: bool,
}

// ---------------------------------------------------------------------------
// The inline option state
// ---------------------------------------------------------------------------

/// The inline-mutable subset of the compile options (`(?imsxU)` and friends).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Opts {
    caseless: bool,
    dotall: bool,
    multiline: bool,
    extended: bool,
    ungreedy: bool,
    dollar_endonly: bool,
}

impl Opts {
    fn from_flags(flags: &CompileFlags) -> Opts {
        Opts {
            caseless: flags.caseless,
            dotall: flags.dotall,
            multiline: flags.multiline,
            extended: flags.extended,
            ungreedy: flags.ungreedy,
            dollar_endonly: flags.dollar_endonly,
        }
    }
}

// ---------------------------------------------------------------------------
// ASCII class helpers
// ---------------------------------------------------------------------------

fn ranges_of(ranges: &[(u32, u32)]) -> Ranges {
    ranges.to_vec()
}

/// `\d` / `\s` / `\w` (PCRE2's default 8-bit "low-valued" tables: ASCII only,
/// verified against the oracle — bytes ≥ 0x80 are *not* word/digit/space).
fn esc_class(c: u32) -> Option<(Ranges, bool)> {
    let digit = ranges_of(&[(0x30, 0x39)]);
    let space = ranges_of(&[(0x09, 0x0D), (0x20, 0x20)]);
    let word = ranges_of(&[(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)]);
    // `\h` horizontal space, `\v` vertical space (PCRE2's 8-bit tables).
    let hspace = ranges_of(&[(0x09, 0x09), (0x20, 0x20), (0xA0, 0xA0)]);
    let vspace = ranges_of(&[(0x0A, 0x0D), (0x85, 0x85)]);
    Some(match c {
        x if x == b'd' as u32 => (digit, false),
        x if x == b'D' as u32 => (digit, true),
        x if x == b's' as u32 => (space, false),
        x if x == b'S' as u32 => (space, true),
        x if x == b'w' as u32 => (word, false),
        x if x == b'W' as u32 => (word, true),
        x if x == b'h' as u32 => (hspace, false),
        x if x == b'H' as u32 => (hspace, true),
        x if x == b'v' as u32 => (vspace, false),
        x if x == b'V' as u32 => (vspace, true),
        _ => return None,
    })
}

/// A POSIX class name → its ASCII member set (`PT_*` in `pcre2_tables.c`).
fn posix_class(name: &str) -> Option<Ranges> {
    let digits = (0x30, 0x39);
    let upper = (0x41, 0x5A);
    let lower = (0x61, 0x7A);
    Some(match name {
        "alpha" => ranges_of(&[upper, lower]),
        "digit" => ranges_of(&[digits]),
        "alnum" => ranges_of(&[digits, upper, lower]),
        "upper" => ranges_of(&[upper]),
        "lower" => ranges_of(&[lower]),
        "space" => ranges_of(&[(0x09, 0x0D), (0x20, 0x20)]),
        "blank" => ranges_of(&[(0x09, 0x09), (0x20, 0x20)]),
        "cntrl" => ranges_of(&[(0x00, 0x1F), (0x7F, 0x7F)]),
        "print" => ranges_of(&[(0x20, 0x7E)]),
        "graph" => ranges_of(&[(0x21, 0x7E)]),
        "punct" => ranges_of(&[(0x21, 0x2F), (0x3A, 0x40), (0x5B, 0x60), (0x7B, 0x7E)]),
        "xdigit" => ranges_of(&[digits, (0x41, 0x46), (0x61, 0x66)]),
        "word" => ranges_of(&[digits, upper, (0x5F, 0x5F), lower]),
        "ascii" => ranges_of(&[(0x00, 0x7F)]),
        _ => return None,
    })
}

/// The maximum codepoint of the subject alphabet.
fn codepoint_max(unicode: bool) -> u32 {
    if unicode {
        0x10FFFF
    } else {
        0xFF
    }
}

// ---------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------

/// Parses a pattern.
///
/// `codes` are the pattern's codepoints, `offs[i]` the byte offset of `codes[i]`
/// in the raw pattern and `total_bytes` the raw pattern's byte length (so an
/// end-of-pattern error reports the position just past the last byte).
///
/// # Errors
///
/// [`ParseError`] for a malformed pattern; the typed `unsupported_bif` for a
/// construct outside the supported subset.
pub(crate) fn parse(
    codes: &[u32],
    offs: &[usize],
    total_bytes: usize,
    flags: &CompileFlags,
) -> beam_common::Result<Syntax> {
    if flags.ucp {
        // `ucp` switches `\d\w\s` and the POSIX classes to Unicode property
        // tables; the engine has **no** Unicode property tables, so EVERY `[ucp]`
        // pattern is the typed `unsupported_bif` capability gap (an honest gap
        // beats a Latin-1-only class that silently under-matches).
        return Err(unsupported_bif());
    }
    if flags.never_utf && flags.unicode {
        return Err(ParseError::new(E_UTF_DISABLED, 0).into());
    }
    let mut p = Parser {
        codes,
        offs,
        total_bytes,
        pos: 0,
        flags,
        opts: Opts::from_flags(flags),
        ncap: 0,
        names: Vec::new(),
        has_cr_or_lf: false,
        depth: 0,
    };
    let ast = p.parse_alternation()?;
    if p.pos < p.codes.len() {
        // A `)` with no matching `(`.
        p.bump();
        return Err(ParseError::new(E_UNMATCHED_PAREN, p.byte_pos(p.pos)).into());
    }
    Ok(Syntax {
        ast,
        ncap: p.ncap,
        names: p.names,
        has_cr_or_lf: p.has_cr_or_lf,
    })
}

impl From<ParseError> for beam_common::BeamError {
    /// Wraps a compile error as a raise whose **opaque live-reason slot** carries
    /// the [`ParseError`] value.
    ///
    /// A compile failure is a *value* in `re:compile/1,2` (the `{error, {Msg,
    /// Pos}}` tuple the BIF returns), never an observable raise; the BIF layer
    /// recovers it from [`Raise::live_reason`](beam_common::Raise::live_reason)
    /// and renders the tuple. Carrying it in the existing slot keeps the channel
    /// typed without inventing an error variant or a placeholder atom.
    fn from(e: ParseError) -> beam_common::BeamError {
        beam_common::BeamError::Raise(
            beam_common::Raise::error(beam_common::ExitReason::Structured)
                .with_live_reason(beam_common::LiveReason::new(e)),
        )
    }
}

const MAX_DEPTH: u32 = 200;

/// The largest bound a `{n,m}` quantifier may carry.
///
/// PCRE2's `REPETITION_MAX` is `65535`; a larger bound is the oracle's
/// `{"number too big in {} quantifier", Pos}` (verified live: `a{65535}` compiles
/// and `a{65536}` does not).
const QUANT_MAX: u32 = 65535;

struct Parser<'a> {
    codes: &'a [u32],
    offs: &'a [usize],
    total_bytes: usize,
    pos: usize,
    flags: &'a CompileFlags,
    opts: Opts,
    ncap: u32,
    names: Vec<(String, u32)>,
    has_cr_or_lf: bool,
    depth: u32,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u32> {
        self.codes.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u32> {
        self.codes.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<u32> {
        let c = self.codes.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: u32) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn byte_pos(&self, idx: usize) -> usize {
        self.offs.get(idx).copied().unwrap_or(self.total_bytes)
    }

    /// The current error position (the byte offset just past the last consumed
    /// codepoint).
    fn here(&self) -> usize {
        self.byte_pos(self.pos)
    }

    /// Skips whitespace and `#` comments under `extended`.
    fn skip_extended(&mut self) {
        if !self.opts.extended {
            return;
        }
        loop {
            match self.peek() {
                Some(c) if c == 0x20 || c == 0x09 || c == 0x0A || c == 0x0B || c == 0x0C || c == 0x0D => {
                    self.pos += 1;
                }
                Some(c) if c == b'#' as u32 => {
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == 0x0A {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn parse_alternation(&mut self) -> beam_common::Result<Ast> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some(b'|' as u32) {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap_or(Ast::Empty))
        } else {
            Ok(Ast::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> beam_common::Result<Ast> {
        let mut items: Vec<Ast> = Vec::new();
        loop {
            self.skip_extended();
            match self.peek() {
                None => break,
                Some(c) if c == b'|' as u32 || c == b')' as u32 => break,
                _ => {}
            }
            let atom = self.parse_repeat()?;
            // A zero-width no-op group (`(?i)`, `(?#…)`) parses to `Ast::Empty`;
            // dropping it from the concatenation is exactly equivalent (an empty
            // atom matches the empty string) and keeps the item list free of
            // non-items.
            if !matches!(atom, Ast::Empty) {
                items.push(atom);
            }
        }
        Ok(match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap_or(Ast::Empty),
            _ => Ast::Concat(items),
        })
    }

    /// Parses one atom plus any trailing quantifier.
    fn parse_repeat(&mut self) -> beam_common::Result<Ast> {
        let atom = self.parse_atom()?;
        // A lookaround/other zero-width pseudo-atom is never quantifiable, but
        // every atom this parser produces is, so the quantifier check is plain.
        self.skip_extended();
        let (min, max, possessive) = match self.quantifier_spec()? {
            Some(spec) => spec,
            None => return Ok(atom),
        };
        let greedy = !self.opts.ungreedy;
        let (greedy, possessive) = match self.peek() {
            Some(c) if c == b'?' as u32 => {
                self.pos += 1;
                (!greedy, possessive)
            }
            Some(c) if c == b'+' as u32 => {
                self.pos += 1;
                (greedy, true)
            }
            _ => (greedy, possessive),
        };
        // A second quantifier is an error at the position just past it. The
        // oracle parses the offending quantifier in full first, so a bad bound in
        // the *second* brace group (`a{2}{65536}`, `a{2}{2,1}`) reports its own
        // `{}` error at the closing brace, not "nothing to repeat".
        match self.peek() {
            Some(c) if c == b'*' as u32 || c == b'+' as u32 || c == b'?' as u32 => {
                self.pos += 1;
                return Err(ParseError::new(E_NOTHING_TO_REPEAT, self.here()).into());
            }
            Some(c) if c == b'{' as u32 => {
                if self.brace_quantifier()?.is_some() {
                    return Err(ParseError::new(E_NOTHING_TO_REPEAT, self.here()).into());
                }
                // Not a quantifier (`{}`, `{,}`, `{x}`): a literal brace, no error.
            }
            _ => {}
        }
        Ok(Ast::Repeat {
            item: Box::new(atom),
            min,
            max,
            greedy,
            possessive,
        })
    }

    /// Reads a quantifier, if one starts here.
    ///
    /// Returns `None` when the next token is not a quantifier (so the caller
    /// keeps the bare atom).
    fn quantifier_spec(&mut self) -> beam_common::Result<Option<(u32, Option<u32>, bool)>> {
        match self.peek() {
            Some(c) if c == b'*' as u32 => {
                self.pos += 1;
                Ok(Some((0, None, false)))
            }
            Some(c) if c == b'+' as u32 => {
                self.pos += 1;
                Ok(Some((1, None, false)))
            }
            Some(c) if c == b'?' as u32 => {
                self.pos += 1;
                Ok(Some((0, Some(1), false)))
            }
            Some(c) if c == b'{' as u32 => self.brace_quantifier(),
            _ => Ok(None),
        }
    }

    /// Parses `{n}`, `{n,}`, `{,m}` or `{n,m}`.
    ///
    /// Returns `Ok(None)` — with the cursor restored to the `{` — when the braces
    /// do **not** form a quantifier (`{}`, `{,}`, `{x}`, an unterminated `{…`), so
    /// the caller keeps `{` as a literal (PCRE2 10.43+). A non-quantifier brace is
    /// never an error.
    ///
    /// # Errors
    ///
    /// The oracle's own `{}` errors, at the **exact byte offsets PCRE2 reports**
    /// (`pcre2_compile.c:read_repeat_counts`): [`E_NUMBER_TOO_BIG`] when a bound is
    /// above [`QUANT_MAX`], reported just past that number's last digit, and
    /// [`E_NUMBERS_OUT_OF_ORDER`] when the maximum is below the minimum, likewise
    /// reported just past the maximum's last digit. The too-big check precedes the
    /// order check.
    ///
    /// Whitespace inside the braces (`a{ 2 }`, which PCRE2 allows) is **not**
    /// supported — the pre-check simply fails and the braces stay literal.
    fn brace_quantifier(&mut self) -> beam_common::Result<Option<(u32, Option<u32>, bool)>> {
        let start = self.pos; // the `{`
        // --- PCRE2's syntax pre-check (the `pp` walk, pcre2_compile.c:1369-1395):
        // decide *whether* this is a quantifier before interpreting any number, so
        // that e.g. `X{123456ABC` is a literal, not a "number too big" error.
        let (_, after_min, had_min) = self.digits_at(start + 1);
        let pp = after_min;
        match self.codes.get(pp) {
            Some(&c) if c == b'}' as u32 => {
                if !had_min {
                    return Ok(None); // `{}`
                }
            }
            Some(&c) if c == b',' as u32 => {
                let (_, after_max, had_max) = self.digits_at(pp + 1);
                if self.codes.get(after_max) != Some(&(b'}' as u32)) {
                    return Ok(None); // `{n,m,}` / an unterminated brace
                }
                if !had_max && !had_min {
                    return Ok(None); // `{,}`
                }
            }
            _ => return Ok(None), // `{x}`, `{…` (unterminated), end of pattern
        }

        // --- Interpret for real (pcre2_compile.c:1397-1447). `self.pos` moves only
        // on success; the error offsets are the indices PCRE2's `read_number`
        // leaves `ptr` at (just past the last digit of the offending number).
        let (min_val, after_min, _) = self.digits_at(start + 1);
        if min_val > u64::from(QUANT_MAX) {
            return Err(ParseError::new(E_NUMBER_TOO_BIG, self.byte_pos(after_min)).into());
        }
        if self.codes.get(after_min) == Some(&(b'}' as u32)) {
            // `{n}`
            self.pos = after_min + 1;
            let n = min_val as u32;
            return Ok(Some((n, Some(n), false)));
        }
        // The pre-check guarantees a `,` here (`{n,}` / `{n,m}` / `{,m}`).
        let q = after_min + 1;
        if self.codes.get(q) == Some(&(b'}' as u32)) {
            // `{n,}` — an unbounded maximum.
            self.pos = q + 1;
            return Ok(Some((min_val as u32, None, false)));
        }
        let (max_val, after_max, _) = self.digits_at(q);
        if max_val > u64::from(QUANT_MAX) {
            return Err(ParseError::new(E_NUMBER_TOO_BIG, self.byte_pos(after_max)).into());
        }
        if max_val < min_val {
            // The pre-check guarantees a `}` after the maximum.
            return Err(ParseError::new(E_NUMBERS_OUT_OF_ORDER, self.byte_pos(after_max)).into());
        }
        self.pos = after_max + 1; // `}`
        Ok(Some((min_val as u32, Some(max_val as u32), false)))
    }

    /// Reads a decimal run at `from`.
    ///
    /// Returns `(value, index just past the last digit, any)`. The value saturates
    /// at `1_000_000`; PCRE2's `read_number` overflow guard still consumes every
    /// digit before erroring, so the returned end index is always the true end of
    /// the number (which is what the error offset is taken from).
    fn digits_at(&self, from: usize) -> (u64, usize, bool) {
        let mut i = from;
        let mut value: u64 = 0;
        let mut any = false;
        while let Some(&c) = self.codes.get(i) {
            if !(0x30..=0x39).contains(&c) {
                break;
            }
            any = true;
            value = (value * 10 + u64::from(c - 0x30)).min(1_000_000);
            i += 1;
        }
        (value, i, any)
    }

    fn parse_atom(&mut self) -> beam_common::Result<Ast> {
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(Ast::Empty),
        };
        match c {
            x if x == b'(' as u32 => self.parse_group(),
            x if x == b'[' as u32 => self.parse_class(),
            x if x == b'.' as u32 => {
                self.pos += 1;
                Ok(Ast::AnyChar(self.opts.dotall))
            }
            x if x == b'^' as u32 => {
                self.pos += 1;
                Ok(Ast::Assert(Assert::StartLine {
                    multiline: self.opts.multiline,
                }))
            }
            x if x == b'$' as u32 => {
                self.pos += 1;
                Ok(Ast::Assert(Assert::EndLine {
                    multiline: self.opts.multiline,
                    dollar_endonly: self.opts.dollar_endonly,
                }))
            }
            x if x == b'\\' as u32 => self.parse_escape(false),
            x if x == b'{' as u32 => {
                // A `{…}` that forms a quantifier has nothing before it to
                // repeat; the oracle reports that at the end of the quantifier.
                if self.brace_quantifier()?.is_some() {
                    return Err(ParseError::new(E_NOTHING_TO_REPEAT, self.here()).into());
                }
                // `{}`, `{,}`, `{x}`: a literal `{`.
                self.pos += 1;
                self.note_cr_or_lf(c);
                Ok(Ast::Char(c, self.opts.caseless))
            }
            x if x == b'*' as u32 || x == b'+' as u32 || x == b'?' as u32 => {
                self.pos += 1;
                Err(ParseError::new(E_NOTHING_TO_REPEAT, self.here()).into())
            }
            x if x == b')' as u32 => {
                self.pos += 1;
                Err(ParseError::new(E_UNMATCHED_PAREN, self.here()).into())
            }
            _ => {
                self.pos += 1;
                self.note_cr_or_lf(c);
                Ok(Ast::Char(c, self.opts.caseless))
            }
        }
    }

    fn note_cr_or_lf(&mut self, c: u32) {
        if c == 0x0A || c == 0x0D {
            self.has_cr_or_lf = true;
        }
    }

    // -- groups ------------------------------------------------------------

    fn parse_group(&mut self) -> beam_common::Result<Ast> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError::new(E_TOO_DEEP, self.here()).into());
        }
        let result = self.parse_group_inner();
        self.depth -= 1;
        result
    }

    fn parse_group_inner(&mut self) -> beam_common::Result<Ast> {
        self.pos += 1; // `(`
        if !self.eat(b'?' as u32) {
            let group = self.open_capture(None)?;
            return self.finish_group(group);
        }
        match self.peek() {
            Some(c) if c == b':' as u32 => {
                self.pos += 1;
                self.inline_group_body()
            }
            Some(c) if c == b'>' as u32 => {
                self.pos += 1;
                Ok(Ast::Atomic(Box::new(self.inline_group_body()?)))
            }
            Some(c) if c == b'#' as u32 => {
                self.pos += 1;
                while let Some(c) = self.bump() {
                    if c == b')' as u32 {
                        return Ok(Ast::Empty);
                    }
                }
                Err(ParseError::new(E_BAD_COMMENT, self.here()).into())
            }
            Some(c) if c == b'<' as u32 || c == b'\'' as u32 || c == b'P' as u32 => {
                self.parse_named_group_start()
            }
            Some(c) if c == b'=' as u32
                || c == b'!' as u32
                || (c == b'<' as u32)
                || (c == b'|' as u32)
                || (c == b'(' as u32) =>
            {
                Err(unsupported_bif())
            }
            Some(_) => self.parse_inline_options(),
            None => Err(ParseError::new(E_MISSING_PAREN, self.here()).into()),
        }
    }

    /// Parses the name in `(?<name>`, `(?'name'` or `(?P<name>` / `(?P=name)`.
    fn parse_named_group_start(&mut self) -> beam_common::Result<Ast> {
        // `(?<=…)` / `(?<!…)`: a lookbehind `(?<` shares its prefix with a named
        // group, so it must be refused here — before `=`/`!` is read as a name.
        // A lookbehind is outside the supported subset (the same typed gap the
        // `(?=`/`(?!` lookaheads take).
        if self.peek() == Some(b'<' as u32)
            && matches!(self.peek_at(1), Some(c) if c == b'=' as u32 || c == b'!' as u32)
        {
            return Err(unsupported_bif());
        }
        let quote = self.peek();
        let mut backref = false;
        let close = if quote == Some(b'<' as u32) {
            self.pos += 1;
            b'>' as u32
        } else if quote == Some(b'\'' as u32) {
            self.pos += 1;
            b'\'' as u32
        } else {
            // `(?P...`
            self.pos += 1;
            match self.peek() {
                Some(c) if c == b'<' as u32 => {
                    self.pos += 1;
                    b'>' as u32
                }
                Some(c) if c == b'=' as u32 => {
                    self.pos += 1;
                    backref = true;
                    b')' as u32
                }
                _ => {
                    self.pos += 1;
                    return Err(ParseError::new("unrecognized character after (?P", self.here()).into());
                }
            }
        };
        let name_start = self.pos;
        let mut name = String::new();
        loop {
            match self.peek() {
                Some(c) if c == close => break,
                Some(_) => {
                    let c = self.bump().unwrap_or(0);
                    push_codepoint(&mut name, c);
                }
                None => {
                    return Err(ParseError::new(E_MISSING_PAREN, self.here()).into());
                }
            }
        }
        if name.is_empty() {
            return Err(ParseError::new(E_NAME_EXPECTED, self.byte_pos(name_start)).into());
        }
        if name.as_bytes()[0].is_ascii_digit() {
            return Err(ParseError::new(E_NAME_DIGIT, self.byte_pos(name_start) + 1).into());
        }
        if backref {
            self.pos += 1; // `)`
            let index = self
                .names
                .iter()
                .find(|(n, _)| n == &name)
                .map(|(_, i)| *i)
                .ok_or_else(|| ParseError::new(E_BAD_REFERENCE, self.here()))?;
            return Ok(Ast::BackRef { group: index, caseless: self.opts.caseless });
        }
        self.pos += 1; // `>`
        let close_pos = self.here();
        let group = self.open_capture(Some((name, close_pos)))?;
        self.finish_group(group)
    }

    /// Registers a new capture group (or a non-capturing one when `no_auto_capture`).
    fn open_capture(&mut self, name: Option<(String, usize)>) -> beam_common::Result<Option<u32>> {
        if name.is_none() && self.flags.no_auto_capture {
            return Ok(None);
        }
        self.ncap += 1;
        let index = self.ncap;
        if let Some((name, close_pos)) = name {
            if self.names.iter().any(|(n, _)| n == &name) && !self.flags.dupnames {
                return Err(ParseError::new(E_NAME_DUP, close_pos).into());
            }
            self.names.push((name, index));
        }
        Ok(Some(index))
    }

    fn finish_group(&mut self, index: Option<u32>) -> beam_common::Result<Ast> {
        let inner = self.inline_group_body()?;
        Ok(match index {
            Some(index) => Ast::Capture {
                index,
                item: Box::new(inner),
            },
            None => inner,
        })
    }

    /// Parses a group body with the inline options scoped to it: an option change
    /// inside (`(?i)`) is undone at the group's `)`, exactly as PCRE2's
    /// `META_OPTIONS` nest tracking does ("the option change applies for the
    /// remainder of the enclosing group").
    ///
    /// The unscoped `(?i)` form is deliberately **not** routed through here: it
    /// must leave the options in effect for the remainder of the group it sits
    /// in, which is what its own `)` branch of [`Parser::parse_inline_options`]
    /// does.
    fn inline_group_body(&mut self) -> beam_common::Result<Ast> {
        let saved = self.opts;
        let inner = self.parse_alternation();
        self.opts = saved;
        let inner = inner?;
        self.expect_close_paren()?;
        Ok(inner)
    }

    fn expect_close_paren(&mut self) -> beam_common::Result<()> {
        if self.eat(b')' as u32) {
            Ok(())
        } else {
            Err(ParseError::new(E_MISSING_PAREN, self.here()).into())
        }
    }

    /// `(?i)`, `(?im-sx)`, `(?i:…)`.
    ///
    /// The flag run ends at a `:` (a scoped group: the flags apply only inside it)
    /// or a `)` (an unscoped group: the flags stay in effect for the remainder of
    /// the enclosing group). Anything else is an unknown option letter.
    fn parse_inline_options(&mut self) -> beam_common::Result<Ast> {
        let mut new = self.opts;
        let mut negate = false;
        loop {
            match self.peek() {
                Some(c) if c == b'-' as u32 => {
                    self.pos += 1;
                    negate = true;
                }
                Some(c) if c == b')' as u32 => {
                    self.pos += 1;
                    // An unscoped `(?i)` leaves the flags on for what follows.
                    self.opts = new;
                    return Ok(Ast::Empty);
                }
                Some(c) if c == b':' as u32 => {
                    self.pos += 1;
                    // A scoped `(?i:…)` restores the outer flags after the group.
                    let saved = self.opts;
                    self.opts = new;
                    let inner = self.parse_alternation();
                    self.opts = saved;
                    let inner = inner?;
                    self.expect_close_paren()?;
                    return Ok(inner);
                }
                Some(c) => {
                    let on = !negate;
                    match c {
                        x if x == b'i' as u32 => new.caseless = on,
                        x if x == b's' as u32 => new.dotall = on,
                        x if x == b'm' as u32 => new.multiline = on,
                        x if x == b'x' as u32 => new.extended = on,
                        x if x == b'U' as u32 => new.ungreedy = on,
                        x if x == b'D' as u32 => new.dollar_endonly = on,
                        _ => return Err(ParseError::new(E_BAD_GROUP_OPTION, self.here()).into()),
                    }
                    self.pos += 1;
                }
                None => return Err(ParseError::new(E_MISSING_PAREN, self.here()).into()),
            }
        }
    }

    // -- escapes -----------------------------------------------------------

    /// Parses a `\…` escape outside (`in_class == false`) or inside a class.
    fn parse_escape(&mut self, in_class: bool) -> beam_common::Result<Ast> {
        let bs = self.pos;
        self.pos += 1; // `\`
        let c = match self.peek() {
            Some(c) => c,
            None => return Err(ParseError::new(E_BS_AT_END, self.byte_pos(self.pos)).into()),
        };
        // A quoted literal run `\Q…\E` (also honoured inside a class).
        if c == b'Q' as u32 {
            self.pos += 1;
            let mut items: Vec<Ast> = Vec::new();
            while let Some(c) = self.peek() {
                if c == b'\\' as u32 && self.peek_at(1) == Some(b'E' as u32) {
                    self.pos += 2;
                    break;
                }
                self.pos += 1;
                self.note_cr_or_lf(c);
                items.push(Ast::Char(c, false));
            }
            return Ok(match items.len() {
                0 => Ast::Empty,
                1 => items.pop().unwrap_or(Ast::Empty),
                _ => Ast::Concat(items),
            });
        }
        if c == b'E' as u32 {
            self.pos += 1;
            return Ok(Ast::Empty);
        }
        // A class shorthand.
        if !in_class {
            match c {
                x if x == b'b' as u32 => {
                    self.pos += 1;
                    return Ok(Ast::Assert(Assert::WordBoundary(true)));
                }
                x if x == b'B' as u32 => {
                    self.pos += 1;
                    return Ok(Ast::Assert(Assert::WordBoundary(false)));
                }
                x if x == b'A' as u32 => {
                    self.pos += 1;
                    return Ok(Ast::Assert(Assert::StartSubject));
                }
                x if x == b'z' as u32 => {
                    self.pos += 1;
                    return Ok(Ast::Assert(Assert::EndSubject));
                }
                x if x == b'Z' as u32 => {
                    self.pos += 1;
                    return Ok(Ast::Assert(Assert::EndSubjectOpt));
                }
                x if x == b'G' as u32 => {
                    self.pos += 1;
                    return Ok(Ast::Assert(Assert::StartMatch));
                }
                x if x == b'N' as u32 && self.peek_at(1) != Some(b'{' as u32) => {
                    self.pos += 1;
                    return Ok(Ast::AnyButNewline);
                }
                _ => {}
            }
        }
        if let Some((mut ranges, negate)) = esc_class(c) {
            self.pos += 1;
            if negate {
                ranges_negate(&mut ranges, codepoint_max(self.flags.unicode));
            }
            return Ok(Ast::Class(ranges));
        }
        match c {
            x if x == b'd' as u32
                || x == b'D' as u32
                || x == b's' as u32
                || x == b'S' as u32
                || x == b'w' as u32
                || x == b'W' as u32
                || x == b'h' as u32
                || x == b'H' as u32
                || x == b'v' as u32
                || x == b'V' as u32 =>
            {
                unreachable!("handled by esc_class")
            }
            // A backreference (`\1`..) or an octal escape.
            x if (0x30..=0x39).contains(&x) => self.parse_octal_or_backref(),
            x if x == b'x' as u32 => {
                self.pos += 1;
                let value = self.parse_hex_escape()?;
                self.check_codepoint(value)?;
                self.note_cr_or_lf(value);
                Ok(Ast::Char(value, self.opts.caseless))
            }
            x if x == b'o' as u32 => {
                self.pos += 1;
                let value = self.parse_octal_braces()?;
                self.check_codepoint(value)?;
                self.note_cr_or_lf(value);
                Ok(Ast::Char(value, self.opts.caseless))
            }
            x if x == b'c' as u32 => {
                self.pos += 1;
                let value = match self.bump() {
                    Some(v) if v <= 0x7F => v & 0x1F,
                    Some(_) | None => {
                        return Err(ParseError::new(E_BS_C_AT_END, self.here()).into());
                    }
                };
                Ok(Ast::Char(value, self.opts.caseless))
            }
            x if x == b'k' as u32 => self.parse_named_backref(),
            x if x == b'g' as u32 => self.parse_g_backref(),
            x if x == b'p' as u32 || x == b'P' as u32 || x == b'X' as u32 || x == b'R' as u32 => {
                Err(unsupported_bif())
            }
            x if x == b'K' as u32 => Err(unsupported_bif()),
            x if x == b'C' as u32 => Err(unsupported_bif()),
            x if x == b'F' as u32 || x == b'L' as u32 || x == b'l' as u32 || x == b'U' as u32
                || x == b'u' as u32 =>
            {
                // `\u` is a *literal* `u` in PCRE2 unless in ALT_BSUX mode.
                self.pos += 1;
                Ok(Ast::Char(x, self.opts.caseless))
            }
            x if x == b'n' as u32 => self.named_char(0x0A),
            x if x == b'r' as u32 => self.named_char(0x0D),
            x if x == b't' as u32 => self.named_char(0x09),
            x if x == b'f' as u32 => self.named_char(0x0C),
            x if x == b'a' as u32 => self.named_char(0x07),
            x if x == b'e' as u32 => self.named_char(0x1B),
            x if x == b'N' as u32 => Err(unsupported_bif()),
            _ => {
                // Any other escaped character is that literal character.
                self.pos += 1;
                if !is_punct(c) {
                    return Err(
                        ParseError::new(E_UNRECOGNIZED_ESCAPE, self.byte_pos(self.pos)).into()
                    );
                }
                let _ = bs;
                self.note_cr_or_lf(c);
                Ok(Ast::Char(c, self.opts.caseless))
            }
        }
    }

    fn named_char(&mut self, value: u32) -> beam_common::Result<Ast> {
        self.pos += 1;
        self.note_cr_or_lf(value);
        Ok(Ast::Char(value, self.opts.caseless))
    }

    fn check_codepoint(&self, value: u32) -> beam_common::Result<()> {
        if value > codepoint_max(self.flags.unicode) {
            return Err(ParseError::new(E_CODEPOINT_TOO_LARGE, self.here()).into());
        }
        Ok(())
    }

    /// `\xHH` or `\x{…}` (the leading `x` is already consumed).
    fn parse_hex_escape(&mut self) -> beam_common::Result<u32> {
        if self.eat(b'{' as u32) {
            let mut value: u64 = 0;
            let mut any = false;
            while let Some(c) = self.peek() {
                let d = match (c as u8 as char).to_digit(16) {
                    Some(d) if c <= 0x7F => d,
                    _ => break,
                };
                any = true;
                // Saturate just above every legal maximum (`0x10FFFF`) so the
                // comparison below stays exact without overflowing on an absurdly
                // long digit run.
                value = (value * 16 + u64::from(d)).min(0x11_0000);
                self.pos += 1;
            }
            // PCRE2's `\x{}` diagnostics are reported in this order (verified
            // against the live oracle for `\x{}`, `\x{`, `\x{zz}`, `\x{z}`,
            // `\x{az}`, `\x{za}`, `\x{1z}`, `\x{1`, `\x{4}`, `\x{4z}`, `\x{FF}`,
            // `\x{100}`, `\x{1000}`, `\x{1000000}` and `\x{1234567890123}`):
            //
            // 1. a value above the mode's maximum (`0xFF` without `unicode`,
            //    else `0x10FFFF`), reported at the byte offset where the digit
            //    run stopped — so `\x{123z}` is "too large" *before* the `z` is
            //    ever seen as a non-hex character;
            // 2. a run with no digits at all — the `}` of `\x{}`, or the end of
            //    the pattern for an unterminated `\x{`;
            // 3. a terminator that is not `}` — the "non-hex character" error,
            //    reported at the byte just past the stopping character.
            let stop = self.byte_pos(self.pos);
            if value > u64::from(codepoint_max(self.flags.unicode)) {
                return Err(ParseError::new(E_CODEPOINT_TOO_LARGE, stop).into());
            }
            if !any && (self.peek() == Some(b'}' as u32) || self.peek().is_none()) {
                return Err(ParseError::new(E_DIGITS_MISSING, stop).into());
            }
            if self.eat(b'}' as u32) {
                return Ok(value as u32);
            }
            return Err(ParseError::new(E_HEX_NON_HEX, stop + 1).into());
        }
        let mut value: u32 = 0;
        let mut any = false;
        for _ in 0..2 {
            match self.peek() {
                Some(c) if c <= 0x7F && (c as u8 as char).is_ascii_hexdigit() => {
                    value = value * 16 + (c as u8 as char).to_digit(16).unwrap_or(0);
                    any = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if !any {
            // `\x` with no digits is a NUL (PCRE2's permissive behaviour).
            return Ok(0);
        }
        Ok(value)
    }

    /// `\o{…}` (the leading `o` is already consumed).
    fn parse_octal_braces(&mut self) -> beam_common::Result<u32> {
        if !self.eat(b'{' as u32) {
            return Err(ParseError::new("missing opening brace after \\o", self.here()).into());
        }
        let mut value: u64 = 0;
        let mut any = false;
        while let Some(c) = self.peek() {
            if !(0x30..=0x37).contains(&c) {
                break;
            }
            any = true;
            value = (value * 8 + u64::from(c - 0x30)).min(0x11_0000);
            self.pos += 1;
        }
        if !self.eat(b'}' as u32) || !any {
            return Err(ParseError::new(E_DIGITS_MISSING, self.here()).into());
        }
        if value > 0x10FFFF {
            return Err(ParseError::new(E_CODEPOINT_TOO_LARGE, self.here()).into());
        }
        Ok(value as u32)
    }

    /// `\ddd` — an octal escape, or `\1`.. a backreference.
    fn parse_octal_or_backref(&mut self) -> beam_common::Result<Ast> {
        let first = self.peek().unwrap_or(0);
        if first == b'0' as u32 {
            self.pos += 1;
            let mut value: u32 = 0;
            for _ in 0..2 {
                match self.peek() {
                    Some(c) if (0x30..=0x37).contains(&c) => {
                        value = value * 8 + (c - 0x30);
                        self.pos += 1;
                    }
                    _ => break,
                }
            }
            self.check_codepoint(value)?;
            self.note_cr_or_lf(value);
            return Ok(Ast::Char(value, self.opts.caseless));
        }
        let save = self.pos;
        let digits_start = self.pos;
        let _ = digits_start;
        let mut value: u32 = 0;
        let mut count = 0;
        while count < 3 {
            match self.peek() {
                Some(c) if (0x30..=0x39).contains(&c) => {
                    value = value * 10 + (c - 0x30);
                    count += 1;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        // A reference when it can name a group (PCRE2: single digits always, a
        // longer number when it is not above the capture count seen so far).
        if count == 1 || value <= self.ncap {
            if value > self.ncap {
                return Err(ParseError::new(E_BAD_REFERENCE, self.byte_pos(self.pos)).into());
            }
            return Ok(Ast::BackRef { group: value, caseless: self.opts.caseless });
        }
        // Reinterpret as octal.
        self.pos = save;
        let mut octal: u32 = 0;
        let mut n = 0;
        while n < 3 {
            match self.peek() {
                Some(c) if (0x30..=0x37).contains(&c) => {
                    octal = octal * 8 + (c - 0x30);
                    n += 1;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if n == 0 {
            return Err(ParseError::new(E_BAD_REFERENCE, self.byte_pos(self.pos)).into());
        }
        if !self.flags.unicode && octal > 0xFF {
            return Err(ParseError::new(E_OCTAL_TOO_BIG, self.byte_pos(self.pos)).into());
        }
        self.check_codepoint(octal)?;
        self.note_cr_or_lf(octal);
        Ok(Ast::Char(octal, self.opts.caseless))
    }

    /// `\k<name>`, `\k'name'`, `\k{name}`.
    fn parse_named_backref(&mut self) -> beam_common::Result<Ast> {
        self.pos += 1; // `k`
        let close = match self.peek() {
            Some(c) if c == b'<' as u32 => b'>' as u32,
            Some(c) if c == b'\'' as u32 => b'\'' as u32,
            Some(c) if c == b'{' as u32 => b'}' as u32,
            _ => {
                return Err(ParseError::new(
                    "\\k is not followed by a braced, angle-bracketed, or quoted name",
                    self.here(),
                )
                .into());
            }
        };
        self.pos += 1;
        let name = self.read_name_until(close)?;
        if name.is_empty() {
            return Err(ParseError::new(E_NAME_EXPECTED, self.here()).into());
        }
        let index = self
            .names
            .iter()
            .find(|(n, _)| n == &name)
            .map(|(_, i)| *i)
            .ok_or_else(|| ParseError::new(E_BAD_REFERENCE, self.here()))?;
        Ok(Ast::BackRef { group: index, caseless: self.opts.caseless })
    }

    /// `\g{…}` / `\g<…>` / `\g'…'` / `\g-N`.
    fn parse_g_backref(&mut self) -> beam_common::Result<Ast> {
        self.pos += 1; // `g`
        match self.peek() {
            Some(c) if c == b'{' as u32 || c == b'<' as u32 || c == b'\'' as u32 => {
                let close = match self.bump() {
                    Some(c) if c == b'{' as u32 => b'}' as u32,
                    Some(c) if c == b'<' as u32 => b'>' as u32,
                    _ => b'\'' as u32,
                };
                let name = self.read_name_until(close)?;
                if name.is_empty() {
                    return Err(ParseError::new(E_NAME_EXPECTED, self.here()).into());
                }
                if name.bytes().all(|b| b.is_ascii_digit()) {
                    let n: u32 = name.parse().unwrap_or(0);
                    if n == 0 || n > self.ncap {
                        return Err(ParseError::new(E_BAD_REFERENCE, self.here()).into());
                    }
                    return Ok(Ast::BackRef { group: n, caseless: self.opts.caseless });
                }
                let index = self
                    .names
                    .iter()
                    .find(|(n, _)| n == &name)
                    .map(|(_, i)| *i)
                    .ok_or_else(|| ParseError::new(E_BAD_REFERENCE, self.here()))?;
                Ok(Ast::BackRef { group: index, caseless: self.opts.caseless })
            }
            _ => Err(ParseError::new(
                "\\g is not followed by a braced, angle-bracketed, or quoted name/number or by a plain number",
                self.here(),
            )
            .into()),
        }
    }

    fn read_name_until(&mut self, close: u32) -> beam_common::Result<String> {
        let mut name = String::new();
        loop {
            match self.peek() {
                Some(c) if c == close => {
                    self.pos += 1;
                    return Ok(name);
                }
                Some(_) => {
                    let c = self.bump().unwrap_or(0);
                    push_codepoint(&mut name, c);
                }
                None => return Err(ParseError::new(E_MISSING_PAREN, self.here()).into()),
            }
        }
    }

    // -- character classes -------------------------------------------------

    fn parse_class(&mut self) -> beam_common::Result<Ast> {
        self.pos += 1; // `[`
        let mut ranges: Ranges = Vec::new();
        let negate = self.eat(b'^' as u32);
        let mut first = true;
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => {
                    return Err(ParseError::new(E_MISSING_BRACKET, self.here()).into());
                }
            };
            if c == b']' as u32 && !first {
                self.pos += 1;
                break;
            }
            first = false;
            // POSIX `[[:name:]]`.
            if c == b'[' as u32 && self.peek_at(1) == Some(b':' as u32) {
                self.pos += 2;
                let mut name = String::new();
                loop {
                    match self.peek() {
                        Some(c) if c == b':' as u32 => break,
                        Some(_) => {
                            let c = self.bump().unwrap_or(0);
                            push_codepoint(&mut name, c);
                        }
                        None => {
                            return Err(ParseError::new(E_MISSING_BRACKET, self.here()).into());
                        }
                    }
                }
                self.pos += 1; // `:`
                if !self.eat(b']' as u32) {
                    return Err(ParseError::new(E_MISSING_BRACKET, self.here()).into());
                }
                match posix_class(&name) {
                    Some(mut members) => ranges.append(&mut members),
                    None => {
                        return Err(ParseError::new(E_UNKNOWN_POSIX, self.here()).into());
                    }
                }
                continue;
            }
            let lo = self.class_char()?;
            if let ClassChar::Set(mut set) = lo {
                ranges.append(&mut set);
                continue;
            }
            let lo_char = match lo {
                ClassChar::Char(c) => c,
                ClassChar::Set(_) => unreachable!(),
            };
            // A range?
            if self.peek() == Some(b'-' as u32)
                && self.peek_at(1).is_some()
                && self.peek_at(1) != Some(b']' as u32)
            {
                self.pos += 1; // `-`
                let hi = self.class_char()?;
                match hi {
                    ClassChar::Char(hi_char) => {
                        if hi_char < lo_char {
                            return Err(
                                ParseError::new(E_RANGE_OUT_OF_ORDER, self.here()).into()
                            );
                        }
                        ranges_add(&mut ranges, lo_char, hi_char);
                    }
                    ClassChar::Set(_) => {
                        return Err(
                            ParseError::new(E_ESCAPE_IN_CLASS, self.byte_pos(self.pos)).into()
                        );
                    }
                }
            } else {
                ranges_add(&mut ranges, lo_char, lo_char);
            }
        }
        if self.opts.caseless {
            ranges_add_folds(&mut ranges);
        }
        if negate {
            ranges_negate(&mut ranges, codepoint_max(self.flags.unicode));
        }
        Ok(Ast::Class(ranges))
    }

    /// Reads one class member (a literal, an escape, or a nested set).
    fn class_char(&mut self) -> beam_common::Result<ClassChar> {
        let c = match self.peek() {
            Some(c) => c,
            None => return Err(ParseError::new(E_MISSING_BRACKET, self.here()).into()),
        };
        if c != b'\\' as u32 {
            self.pos += 1;
            self.note_cr_or_lf(c);
            return Ok(ClassChar::Char(c));
        }
        // An escape inside a class: a shorthand yields a set, otherwise a char.
        self.pos += 1;
        let e = match self.peek() {
            Some(c) => c,
            None => return Err(ParseError::new(E_BS_AT_END, self.byte_pos(self.pos)).into()),
        };
        if let Some((mut set, negate)) = esc_class(e) {
            self.pos += 1;
            if negate {
                ranges_negate(&mut set, codepoint_max(self.flags.unicode));
            }
            return Ok(ClassChar::Set(set));
        }
        if e == b'Q' as u32 {
            // `\Q…\E` quoting inside a class: every member is a literal.
            self.pos += 1;
            let mut set: Ranges = Vec::new();
            while let Some(c) = self.peek() {
                if c == b'\\' as u32 && self.peek_at(1) == Some(b'E' as u32) {
                    self.pos += 2;
                    break;
                }
                self.pos += 1;
                self.note_cr_or_lf(c);
                set.push((c, c));
            }
            ranges_normalize(&mut set);
            return Ok(ClassChar::Set(set));
        }
        // Reuse the outer escape parser for a single character.
        self.pos -= 1;
        let ast = self.parse_escape(true)?;
        match ast {
            Ast::Char(c, _) => {
                self.note_cr_or_lf(c);
                Ok(ClassChar::Char(c))
            }
            Ast::Empty => Err(ParseError::new(E_ESCAPE_IN_CLASS, self.here()).into()),
            Ast::Class(set) => {
                if set.iter().any(|&(lo, hi)| lo <= 0x0D && hi >= 0x0A) {
                    self.has_cr_or_lf = true;
                }
                Ok(ClassChar::Set(set))
            }
            _ => Err(ParseError::new(E_ESCAPE_IN_CLASS, self.here()).into()),
        }
    }
}

enum ClassChar {
    Char(u32),
    Set(Ranges),
}

fn push_codepoint(out: &mut String, c: u32) {
    if let Some(ch) = char::from_u32(c) {
        out.push(ch);
    } else {
        // A surrogate cannot appear in a name; keep the name lexically stable.
        out.push('\u{FFFD}');
    }
}

fn is_punct(c: u32) -> bool {
    matches!(c, 0x21..=0x2F | 0x3A..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E) || c > 0x7F
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::re::options::{CapType, CompileFlags};

    fn codes(pattern: &str) -> (Vec<u32>, Vec<usize>, usize) {
        let codes: Vec<u32> = pattern.chars().map(|c| c as u32).collect();
        let offs: Vec<usize> = pattern.char_indices().map(|(i, _)| i).collect();
        (codes, offs, pattern.len())
    }

    fn parse_ascii(pattern: &str) -> beam_common::Result<Syntax> {
        let (codes, offs, total) = codes(pattern);
        parse(&codes, &offs, total, &CompileFlags::default())
    }

    fn parse_with(pattern: &str, flags: &CompileFlags) -> beam_common::Result<Syntax> {
        let (codes, offs, total) = codes(pattern);
        parse(&codes, &offs, total, flags)
    }

    /// The compile error a pattern raises, recovered from the raise's
    /// **live-reason slot** (the channel `impl From<ParseError> for BeamError`
    /// installs).
    fn parse_error(pattern: &str) -> Option<ParseError> {
        match parse_ascii(pattern) {
            Err(beam_common::BeamError::Raise(raise)) => raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<ParseError>())
                .copied(),
            _ => None,
        }
    }

    #[test]
    fn literals_and_groups_parse() {
        let s = parse_ascii("(a)(b)").expect("parses");
        assert_eq!(s.ncap, 2);
        assert!(matches!(s.ast, Ast::Concat(_)));
        assert_eq!(parse_ascii("").expect("parses").ncap, 0);
        assert_eq!(parse_ascii("(?:a)").expect("parses").ncap, 0);
        assert_eq!(parse_ascii("(?<n>a)(?<m>b)").expect("parses").ncap, 2);
        let s = parse_ascii("(?<n>a)(?<m>b)").expect("parses");
        assert_eq!(s.names.len(), 2);
    }

    #[test]
    fn no_auto_capture_suppresses_unnamed_groups() {
        let flags = CompileFlags {
            no_auto_capture: true,
            ..CompileFlags::default()
        };
        let s = parse_with("(a)(?<n>b)", &flags).expect("parses");
        assert_eq!(s.ncap, 1, "only the named group is a capture");
        assert_eq!(s.names.len(), 1);
    }

    #[test]
    fn quantifiers_cover_their_forms() {
        let s = parse_ascii("a*").expect("parses");
        match s.ast {
            Ast::Repeat {
                min,
                max,
                greedy,
                possessive,
                ..
            } => {
                assert_eq!((min, max, greedy, possessive), (0, None, true, false));
            }
            other => panic!("expected a repeat, got {other:?}"),
        }
        let s = parse_ascii("a*?").expect("parses");
        match s.ast {
            Ast::Repeat { greedy, .. } => assert!(!greedy),
            other => panic!("{other:?}"),
        }
        let s = parse_ascii("a*+").expect("parses");
        match s.ast {
            Ast::Repeat { possessive, .. } => assert!(possessive),
            other => panic!("{other:?}"),
        }
        let s = parse_ascii("a{2,3}").expect("parses");
        match s.ast {
            Ast::Repeat { min, max, .. } => assert_eq!((min, max), (2, Some(3))),
            other => panic!("{other:?}"),
        }
        let s = parse_ascii("a{2,}").expect("parses");
        match s.ast {
            Ast::Repeat { min, max, .. } => assert_eq!((min, max), (2, None)),
            other => panic!("{other:?}"),
        }
        // `{` without a valid quantifier is a literal.
        let s = parse_ascii("a{2").expect("parses");
        assert!(matches!(s.ast, Ast::Concat(_)));
    }

    #[test]
    fn ungreedy_swaps_the_default() {
        let flags = CompileFlags {
            ungreedy: true,
            ..CompileFlags::default()
        };
        let s = parse_with("a+", &flags).expect("parses");
        match s.ast {
            Ast::Repeat { greedy, .. } => assert!(!greedy),
            other => panic!("{other:?}"),
        }
        let s = parse_with("a+?", &flags).expect("parses");
        match s.ast {
            Ast::Repeat { greedy, .. } => assert!(greedy),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classes_are_normalised_and_negated() {
        let s = parse_ascii("[a-c]").expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert_eq!(ranges, vec![(0x61, 0x63)]);
                assert!(ranges_contain(&ranges, 0x62));
                assert!(!ranges_contain(&ranges, 0x64));
            }
            other => panic!("{other:?}"),
        }
        let s = parse_ascii("[^a]").expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert!(!ranges_contain(&ranges, 0x61));
                assert!(ranges_contain(&ranges, 0x00));
                assert!(ranges_contain(&ranges, 0xFF));
            }
            other => panic!("{other:?}"),
        }
        // `]` first is a literal member.
        let s = parse_ascii("[]a]").expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert!(ranges_contain(&ranges, 0x5D));
                assert!(ranges_contain(&ranges, 0x61));
            }
            other => panic!("{other:?}"),
        }
        // POSIX classes.
        let s = parse_ascii("[[:alpha:]]").expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert!(ranges_contain(&ranges, 0x41));
                assert!(ranges_contain(&ranges, 0x7A));
                assert!(!ranges_contain(&ranges, 0x30));
                assert!(!ranges_contain(&ranges, 0xE4), "ASCII only, as the oracle");
            }
            other => panic!("{other:?}"),
        }
        // A class shorthand inside a class.
        let s = parse_ascii("[\\d]").expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert!(ranges_contain(&ranges, 0x35));
                assert!(!ranges_contain(&ranges, 0x41));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn caseless_folds_the_classes() {
        let flags = CompileFlags {
            caseless: true,
            ..CompileFlags::default()
        };
        let s = parse_with("[a]", &flags).expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert!(ranges_contain(&ranges, 0x61));
                assert!(ranges_contain(&ranges, 0x41));
            }
            other => panic!("{other:?}"),
        }
        // A negated caseless class excludes both cases.
        let s = parse_with("[^a]", &flags).expect("parses");
        match s.ast {
            Ast::Class(ranges) => {
                assert!(!ranges_contain(&ranges, 0x61));
                assert!(!ranges_contain(&ranges, 0x41));
                assert!(ranges_contain(&ranges, 0x62));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn escapes_cover_the_documented_set() {
        for (pattern, want) in [
            (r"\n", 0x0Au32),
            (r"\r", 0x0D),
            (r"\t", 0x09),
            (r"\x41", 0x41),
            (r"\x{41}", 0x41),
            (r"\o{101}", 0x41),
            (r"\101", 0x41),
            (r"\0", 0x00),
            (r"\cA", 0x01),
            (r"\.", 0x2E),
            (r"\\", 0x5C),
        ] {
            let s = parse_ascii(pattern).unwrap_or_else(|e| panic!("{pattern}: {e:?}"));
            match s.ast {
                Ast::Char(c, _) => assert_eq!(c, want, "{pattern}"),
                other => panic!("{pattern}: expected a char, got {other:?}"),
            }
        }
        // A quoted run is literal.
        let s = parse_ascii(r"\Qa+b\E").expect("parses");
        match s.ast {
            Ast::Concat(items) => assert_eq!(items.len(), 3),
            other => panic!("{other:?}"),
        }
        // An unrecognised escape is an error.
        assert!(parse_ascii(r"\q").is_err());
    }

    #[test]
    fn anchors_and_boundaries_parse() {
        assert!(matches!(
            parse_ascii(r"\b").expect("parses").ast,
            Ast::Assert(Assert::WordBoundary(true))
        ));
        assert!(matches!(
            parse_ascii(r"\B").expect("parses").ast,
            Ast::Assert(Assert::WordBoundary(false))
        ));
        for p in [r"\A", r"\z", r"\Z", r"\G", "^", "$"] {
            assert!(matches!(parse_ascii(p).expect("parses").ast, Ast::Assert(_)), "{p}");
        }
    }

    #[test]
    fn backreferences_parse() {
        let s = parse_ascii(r"(a)\1").expect("parses");
        match s.ast {
            Ast::Concat(items) => assert!(matches!(items[1], Ast::BackRef { group: 1, caseless: false })),
            other => panic!("{other:?}"),
        }
        let s = parse_ascii(r"(?<n>a)\k<n>").expect("parses");
        match s.ast {
            Ast::Concat(items) => assert!(matches!(items[1], Ast::BackRef { group: 1, caseless: false })),
            other => panic!("{other:?}"),
        }
        let s = parse_ascii(r"(?<n>a)\g{n}").expect("parses");
        match s.ast {
            Ast::Concat(items) => assert!(matches!(items[1], Ast::BackRef { group: 1, caseless: false })),
            other => panic!("{other:?}"),
        }
        // A reference to a group that does not exist is an error.
        assert!(parse_ascii(r"\1").is_err());
    }

    #[test]
    fn inline_option_groups_change_the_scope() {
        let s = parse_ascii("(?i)abc").expect("parses");
        match s.ast {
            Ast::Concat(items) => {
                assert!(items.iter().all(|i| matches!(i, Ast::Char(_, true))));
            }
            other => panic!("{other:?}"),
        }
        let s = parse_ascii("a(?i:BC)").expect("parses");
        match s.ast {
            Ast::Concat(items) => {
                assert!(matches!(items[0], Ast::Char(_, false)));
                assert!(matches!(items[1], Ast::Concat(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extended_mode_skips_whitespace_and_comments() {
        let flags = CompileFlags {
            extended: true,
            ..CompileFlags::default()
        };
        let s = parse_with("a  b  c", &flags).expect("parses");
        match s.ast {
            Ast::Concat(items) => assert_eq!(items.len(), 3),
            other => panic!("{other:?}"),
        }
        let s = parse_with("a # comment\nb", &flags).expect("parses");
        match s.ast {
            Ast::Concat(items) => assert_eq!(items.len(), 2),
            other => panic!("{other:?}"),
        }
        // Whitespace inside a class stays significant.
        let s = parse_with("[ ]", &flags).expect("parses");
        match s.ast {
            Ast::Class(ranges) => assert!(ranges_contain(&ranges, 0x20)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_positions_match_the_oracle_goldens() {
        for (pattern, msg, pos) in [
            ("a(b", E_MISSING_PAREN, 3usize),
            ("[a", E_MISSING_BRACKET, 2),
            ("a{2,1}", E_NUMBERS_OUT_OF_ORDER, 5),
        ] {
            let got = parse_error(pattern).unwrap_or_else(|| panic!("{pattern} unexpectedly parsed"));
            assert_eq!(got.msg, msg, "{pattern}");
            assert_eq!(got.pos, pos, "{pattern}");
        }
    }

    #[test]
    fn fold_key_is_the_documented_canonicaliser() {
        assert_eq!(fold_key(0x41), 0x61);
        assert_eq!(fold_key(0x61), 0x61);
        assert_eq!(fold_key(0xC4), 0xE4);
        assert_eq!(fold_key(0xFF), 0x178);
        assert_eq!(fold_key(0x178), 0x178);
        assert_eq!(fold_key(0x3A3), 0x3C3);
        assert_eq!(fold_key(0x3C2), 0x3C3);
        assert!(chars_equal(0x41, 0x61, true));
        assert!(!chars_equal(0x41, 0x61, false));
    }

    #[test]
    fn capture_types_are_exported_for_the_renderer() {
        // `CapType` is re-exported through the options module; touched here so
        // the test module fails to compile if the seam moves.
        let _ = CapType::Index;
    }

    #[test]
    fn a_parse_error_round_trips_through_the_live_reason_slot() {
        // 3a: the compile-error channel is the `Raise` opaque live-reason slot,
        // not a fabricated `re_compile_error` atom.
        let want = ParseError::new(E_MISSING_PAREN, 3);
        match parse_ascii("a(b") {
            Err(beam_common::BeamError::Raise(raise)) => {
                let got = raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<ParseError>())
                    .copied()
                    .expect("the live reason carries the ParseError");
                assert_eq!(got, want);
            }
            other => panic!("expected a Raise, got {other:?}"),
        }
    }

    #[test]
    fn a_ucp_pattern_is_a_distinguishable_typed_gap() {
        // 3a: a construct outside the subset stays the typed `unsupported_bif`
        // (reason atom `unsupported_bif`) and is distinguishable from a compile
        // error carried in the live-reason slot.
        let flags = CompileFlags {
            ucp: true,
            ..CompileFlags::default()
        };
        match parse_with(r"\d", &flags) {
            Err(beam_common::BeamError::Raise(raise)) => {
                assert!(raise.live_reason().is_none());
                assert_eq!(raise.reason.as_atom(), Some("unsupported_bif"));
            }
            other => panic!("expected a typed gap, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_constructs_stay_typed_gaps_and_carry_no_parse_error() {
        for p in [r"(?=a)", r"(?!a)", r"(?<=a)", r"(?<!a)", r"\p{L}", r"\R", r"\X", r"\K", r"(?|a)"] {
            match parse_ascii(p) {
                Err(beam_common::BeamError::Raise(raise)) => {
                    assert!(
                        raise.live_reason().is_none(),
                        "{p} must be a typed gap, not a compile error"
                    );
                }
                other => panic!("{p} must be a typed gap, got {other:?}"),
            }
        }
    }

    #[test]
    fn inline_options_apply_the_flags_they_name() {
        // 3b: `(?i)abc` marks every following char caseless.
        let s = parse_ascii("(?i)abc").expect("parses");
        match s.ast {
            Ast::Concat(items) => {
                assert_eq!(items.len(), 3);
                assert!(items.iter().all(|i| matches!(i, Ast::Char(_, true))), "{items:?}");
            }
            other => panic!("{other:?}"),
        }

        // 3b: `a(?i:BC)` applies caseless only inside the group.
        let s = parse_ascii("a(?i:BC)d").expect("parses");
        match s.ast {
            Ast::Concat(items) => {
                assert_eq!(items.len(), 3);
                assert!(
                    matches!(items[0], Ast::Char(c, false) if c == 0x61),
                    "{:?}",
                    items[0]
                );
                match &items[1] {
                    Ast::Concat(inner) => {
                        assert!(inner.iter().all(|i| matches!(i, Ast::Char(_, true))), "{inner:?}");
                    }
                    other => panic!("expected the group body, got {other:?}"),
                }
                assert!(
                    matches!(items[2], Ast::Char(c, false) if c == 0x64),
                    "{:?}",
                    items[2]
                );
            }
            other => panic!("{other:?}"),
        }

        // A mixed flag run applies each flag it names: the `i` makes the
        // following `a` caseless (the zero-width `(?im-sx)` itself is dropped
        // from the concatenation, so the `a` is the whole pattern).
        let s = parse_ascii("(?im-sx)a").expect("parses");
        assert!(
            matches!(s.ast, Ast::Char(c, true) if c == 0x61),
            "{:?}",
            s.ast
        );
        let s = parse_ascii("(?s)a.b").expect("parses");
        match s.ast {
            Ast::Concat(items) => assert!(matches!(items[1], Ast::AnyChar(true)), "{items:?}"),
            other => panic!("{other:?}"),
        }

        // An unknown option letter is the oracle's error.
        let got = parse_error("(?q)a").expect("error");
        assert_eq!(got.msg, E_BAD_GROUP_OPTION);
    }

    #[test]
    fn inline_options_are_scoped_to_their_enclosing_group() {
        // 6: an unscoped `(?i)` inside a real group applies only for the
        // remainder of that group — the `)` undoes it, exactly as PCRE2's
        // `META_OPTIONS` nest tracking does.
        let s = parse_ascii("((?i)a)b").expect("parses");
        let items = match s.ast {
            Ast::Concat(items) => items,
            other => panic!("{other:?}"),
        };
        assert_eq!(s.ncap, 1);
        assert_eq!(items.len(), 2, "{items:?}");
        match &items[0] {
            Ast::Capture { index, item } => {
                assert_eq!(*index, 1);
                assert!(
                    matches!(**item, Ast::Char(c, true) if c == 0x61),
                    "`a` must be caseless inside the group, got {item:?}"
                );
            }
            other => panic!("expected the capturing group, got {other:?}"),
        }
        assert!(
            matches!(items[1], Ast::Char(c, false) if c == 0x62),
            "the trailing `b` must be case-SENSITIVE after the group, got {:?}",
            items[1]
        );

        // A top-level unscoped `(?i)` still marks every following char caseless.
        let s = parse_ascii("(?i)abc").expect("parses");
        match s.ast {
            Ast::Concat(items) => {
                assert_eq!(items.len(), 3, "{items:?}");
                assert!(items.iter().all(|i| matches!(i, Ast::Char(_, true))), "{items:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn quantifier_errors_match_the_oracle_goldens() {
        // 3d: quantifier errors point at the closing `}` (0-based byte offset).
        for (pattern, msg, pos) in [
            ("{,2}", E_NOTHING_TO_REPEAT, 4usize),
            ("{2}", E_NOTHING_TO_REPEAT, 3),
            ("a{2}{3}", E_NOTHING_TO_REPEAT, 7),
            ("a{1,2}{3}", E_NOTHING_TO_REPEAT, 9),
            ("a{2,3}{4}", E_NOTHING_TO_REPEAT, 9),
            ("a*{2}", E_NOTHING_TO_REPEAT, 5),
            ("a?{2}", E_NOTHING_TO_REPEAT, 5),
            ("a{,2}{3}", E_NOTHING_TO_REPEAT, 8),
            ("a{2}{,2}", E_NOTHING_TO_REPEAT, 8),
            ("a*?*", E_NOTHING_TO_REPEAT, 4),
            ("a**", E_NOTHING_TO_REPEAT, 3),
            ("a**?", E_NOTHING_TO_REPEAT, 3),
            ("a?*", E_NOTHING_TO_REPEAT, 3),
            ("a{2}*", E_NOTHING_TO_REPEAT, 5),
            ("a{2}??", E_NOTHING_TO_REPEAT, 6),
            ("a{2}+?", E_NOTHING_TO_REPEAT, 6),
            ("a{2}{3}?", E_NOTHING_TO_REPEAT, 7),
            ("a{65536}", E_NUMBER_TOO_BIG, 7),
            ("a{1000000}", E_NUMBER_TOO_BIG, 9),
            ("a{,65536}", E_NUMBER_TOO_BIG, 8),
            ("a{,1000000}", E_NUMBER_TOO_BIG, 10),
            ("a{2,65536}", E_NUMBER_TOO_BIG, 9),
            ("a{65535,65536}", E_NUMBER_TOO_BIG, 13),
            // The too-big check precedes the order check.
            ("a{65536,65535}", E_NUMBER_TOO_BIG, 7),
            ("a{2}{65536}", E_NUMBER_TOO_BIG, 10),
            // The second quantifier's own `{}` error wins.
            ("a{2,1}{3}", E_NUMBERS_OUT_OF_ORDER, 5),
        ] {
            let got = parse_error(pattern)
                .unwrap_or_else(|| panic!("{pattern} unexpectedly parsed"));
            assert_eq!(got.msg, msg, "{pattern}");
            assert_eq!(got.pos, pos, "{pattern}");
        }
    }

    #[test]
    fn benign_brace_forms_are_literals_not_quantifiers() {
        // 3d: `{}`/`{,}`, an unterminated `{…` and a non-numeric brace are
        // literal `{` + text, never a quantifier and never an error.
        for pattern in [
            "a{}", "a{,}", "a{,2}", "a{2,}", "a{}{2}", "a{,}{2}", "a{2}{}", "a{2}{,}",
            "a{65536", "a{2,3}x", "a{65536x}", "a{2,1,}", "a{,2,3}", "{}",
        ] {
            assert!(parse_ascii(pattern).is_ok(), "{pattern} must be a literal brace");
        }
        // The boundary itself is accepted: `a{65535}` compiles, `a{65536}` does not.
        assert!(parse_ascii("a{65535}").is_ok());
        assert!(parse_ascii("a{65535,65535}").is_ok());
        assert!(parse_ascii("a{0}").is_ok());
        assert!(parse_ascii("a{0,0}").is_ok());
        // Leading zeros are digits, so `{01}` is a `{1}` quantifier.
        assert!(parse_ascii("a{01}").is_ok());
        // A lone `{` that closes nothing is a literal as well.
        assert!(parse_ascii("a{").is_ok());
    }
}
