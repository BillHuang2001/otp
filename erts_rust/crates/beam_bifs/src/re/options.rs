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

//! Compile-time and run-time option parsing for the `re` engine.
//!
//! A faithful port of `parse_options` (`../../../../erts/emulator/beam/erl_bif_re.c:350-543`):
//! one pass over the option list produces *both* the compile flags and the
//! match flags, because the oracle's `re:compile/2` and `re:run/2,3` share the
//! parser and then reject the half they do not accept
//! (`PARSE_FLAG_UNIQUE_EXEC_OPT` in `compile`, `PARSE_FLAG_UNIQUE_COMPILE_OPT`
//! in `run` on a precompiled pattern, `PARSE_FLAG_EXPORT` in `run`).
//!
//! Anything the oracle rejects with `badarg` is `badarg` here (an unknown option,
//! a malformed option tuple, an improper list, a negative
//! `{offset|match_limit|match_limit_recursion, _}`, a malformed capture spec).
//!
//! # The capture spec
//!
//! `{capture, Values}` / `{capture, Values, Type}` is *not* resolved here into
//! group numbers: a name in the id list must be looked up in the compiled
//! program's name table, and `all_names` needs the whole table, so
//! [`CapValue`] keeps the requested shape and [`crate::re::render`] resolves it
//! against the program (`build_capture`, `erl_bif_re.c:1309-1479`).
//!
//! A negative integer id is `badarg` (oracle-verified: `re:run("xabcy","(b)(c)",
//! [{capture,[-1],index}])` is `error:badarg`), as is a non-integer,
//! non-atom, non-iodata id.

use beam_common::Result;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

use crate::access;
use crate::error::badarg;

/// `{newline, NLSpec}` (`nl_spec()`) — the compiled newline convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Newline {
    /// `lf` — the default (`PCRE2_NEWLINE_LF`).
    #[default]
    Lf,
    /// `cr`.
    Cr,
    /// `crlf`.
    Crlf,
    /// `nul`.
    Nul,
    /// `anycrlf`.
    AnyCrlf,
    /// `any`.
    Any,
}

impl Newline {
    /// Whether the convention counts as CR/CRLF-like for the compiled pattern's
    /// element 4 — the oracle's `use_crlf` (`erl_bif_re.c:597-599`), which is
    /// `CRLF | ANYCRLF | ANY` (and *not* `CR`).
    pub(crate) fn is_crlf_like(self) -> bool {
        matches!(self, Newline::Crlf | Newline::AnyCrlf | Newline::Any)
    }

    /// All characters that *terminate* a line under this convention, in codepoint
    /// order. `\r\n` (CRLF) is handled by [`Newline::newline_len_at`].
    fn line_terminators(self) -> &'static [u32] {
        match self {
            // A lone `\r` is a newline under CR/ANYCRLF/ANY; under CRLF the pair
            // `\r\n` is handled positionally, but a standalone `\n` is still a
            // terminator (`PCRE2_NEWLINE_CRLF` treats both bytes as line ends).
            Newline::Lf | Newline::Crlf => &[10],
            Newline::Cr => &[13],
            Newline::Nul => &[0],
            Newline::AnyCrlf => &[10, 13],
            Newline::Any => &[10, 11, 12, 13, 0x85, 0x2028, 0x2029],
        }
    }

    /// Whether `c` is one of the characters `.` refuses to match (a newline
    /// terminator of this convention).
    pub(crate) fn is_newline_char(self, c: u32) -> bool {
        match self {
            // `PCRE2_NEWLINE_CRLF` excludes both bytes of the pair from `.`.
            Newline::Crlf => c == 10 || c == 13,
            Newline::Lf | Newline::Cr | Newline::Nul | Newline::AnyCrlf | Newline::Any => {
                self.line_terminators().contains(&c)
            }
        }
    }

    /// The number of codepoints of the newline sequence starting at `pos`, or
    /// `None` when no sequence starts there.
    pub(crate) fn newline_len_at(self, codes: &[u32], pos: usize) -> Option<usize> {
        let c = *codes.get(pos)?;
        match self {
            Newline::Crlf => match c {
                13 if codes.get(pos + 1) == Some(&10) => Some(2),
                10 | 13 => Some(1),
                _ => None,
            },
            _ => {
                if self.line_terminators().contains(&c) {
                    Some(1)
                } else {
                    None
                }
            }
        }
    }

    /// Whether a newline sequence of this convention *ends* exactly at `pos` (so
    /// `^` may match there in multiline mode).
    pub(crate) fn ends_at(self, codes: &[u32], pos: usize) -> bool {
        if pos == 0 {
            return false;
        }
        match self {
            // Under the CRLF convention a line ends after the `\n` that closes a
            // `crlf` pair, and also after a lone `\r` or `\n`
            // (`PCRE2_NEWLINE_CRLF` treats both bytes as line ends).
            Newline::Crlf => codes[pos - 1] == 10 || codes[pos - 1] == 13,
            _ => self.line_terminators().contains(&codes[pos - 1]),
        }
    }

    /// The length of the newline sequence ending exactly at `pos`, if any.
    pub(crate) fn ends_at_len(self, codes: &[u32], pos: usize) -> Option<usize> {
        if pos == 0 {
            return None;
        }
        match self {
            Newline::Crlf => {
                if codes[pos - 1] == 13 && codes.get(pos) == Some(&10) {
                    Some(1)
                } else if codes[pos - 1] == 10 {
                    Some(1)
                } else if codes[pos - 1] == 13 {
                    Some(1)
                } else {
                    None
                }
            }
            _ => {
                if self.line_terminators().contains(&codes[pos - 1]) {
                    Some(1)
                } else {
                    None
                }
            }
        }
    }
}

/// The `bsr_*` option (only meaningful for `\R`, which this engine does not
/// implement) — parsed so an unknown/malformed value is still `badarg`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Bsr {
    /// `bsr_anycrlf`.
    AnyCrlf,
    /// `bsr_unicode`.
    Unicode,
}

/// The compile-half options.
#[derive(Clone, Debug, Default)]
pub(crate) struct CompileFlags {
    /// `unicode` (`PCRE2_UTF`).
    pub unicode: bool,
    /// `caseless` (`PCRE2_CASELESS`).
    pub caseless: bool,
    /// `dotall` (`PCRE2_DOTALL`).
    pub dotall: bool,
    /// `multiline` (`PCRE2_MULTILINE`).
    pub multiline: bool,
    /// `extended` (`PCRE2_EXTENDED`).
    pub extended: bool,
    /// `ungreedy` (`PCRE2_UNGREEDY`).
    pub ungreedy: bool,
    /// `dollar_endonly` (`PCRE2_DOLLAR_ENDONLY`).
    pub dollar_endonly: bool,
    /// `firstline` (`PCRE2_FIRSTLINE`).
    pub firstline: bool,
    /// `no_auto_capture` (`PCRE2_NO_AUTO_CAPTURE`).
    pub no_auto_capture: bool,
    /// `dupnames` (`PCRE2_DUPNAMES`).
    pub dupnames: bool,
    /// `ucp` (`PCRE2_UCP`).
    pub ucp: bool,
    /// `never_utf` (`PCRE2_NEVER_UTF`).
    pub never_utf: bool,
    /// `anchored` (`PCRE2_ANCHORED`, set at both compile and match time).
    pub anchored: bool,
    /// `no_start_optimize` (`PCRE2_NO_START_OPTIMIZE`) — accepted, a no-op here.
    pub no_start_optimize: bool,
    /// The newline convention (`{newline, NLSpec}`, possibly set at run time).
    pub newline: Newline,
}

/// One entry of a `{capture, [...]}` id list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CapId {
    /// A group number.
    Index(u32),
    /// A subpattern name.
    Name(String),
}

/// The `ValueSpec` of a capture specification (`capture()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CapValue {
    /// `all` — every captured subpattern including the whole match.
    All,
    /// `all_but_first` — every captured subpattern except the whole match.
    AllButFirst,
    /// `all_names` — the *named* captured subpatterns, in name order.
    AllNames,
    /// `first` — only the whole match.
    First,
    /// `none`, or the empty list `[]` — no captures at all (a bare `match`).
    None,
    /// An explicit list of group numbers / names, in the requested order.
    Names(Vec<CapId>),
}

/// The `Type` of a capture specification (`index | list | binary`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum CapType {
    /// Byte offsets (`{Start, Len}`) — the default.
    #[default]
    Index,
    /// A charlist.
    List,
    /// A binary.
    Binary,
}

/// Everything `parse_options` produces.
#[derive(Clone, Debug, Default)]
pub(crate) struct Parsed {
    /// The compile-half flags.
    pub compile: CompileFlags,
    /// A `unique-compile` option was seen (`compile` must reject a run-only list).
    pub unique_compile: bool,
    /// A `unique-exec` option was seen (`run` on a precompiled RE must reject a
    /// list carrying a run-only option).
    pub unique_exec: bool,
    /// `export`.
    pub export: bool,
    /// `report_errors`.
    pub report_errors: bool,
    /// `global`.
    pub global: bool,
    /// `notempty`.
    pub notempty: bool,
    /// `notempty_atstart`.
    pub notempty_atstart: bool,
    /// `notbol`.
    pub notbol: bool,
    /// `noteol`.
    pub noteol: bool,
    /// A `{capture, …}` specification was given.
    pub capture: Option<(CapValue, CapType)>,
    /// `{offset, N}`.
    pub start_offset: u32,
    /// `{match_limit, N}`.
    pub match_limit: Option<u32>,
    /// `{match_limit_recursion, N}` — accepted and ignored (this engine is not
    /// recursive, so it has no recursion-depth limit to honour).
    pub match_limit_recursion: Option<u32>,
    /// The `{newline, _}` option was given.
    pub newline_opt: Option<Newline>,
    /// The `bsr_*` option was given.
    pub bsr_opt: Option<Bsr>,
}

/// Parses the option list `list` — a faithful `parse_options`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for every shape the
/// oracle's `parse_options` rejects (it reports `badarg` with `cause => badopt`).
pub(crate) fn parse(atoms: &AtomTable, list: Eterm) -> Result<Parsed> {
    let mut out = Parsed::default();
    let mut cur = list;
    while access::is_list(cur) {
        scan_item(atoms, access::car(cur), &mut out)?;
        cur = access::cdr(cur);
    }
    if !access::is_nil(cur) {
        return Err(badarg());
    }
    Ok(out)
}

/// The atom name of `term`, if it is an atom.
fn atom_name<'a>(atoms: &'a AtomTable, term: Eterm) -> Option<&'a str> {
    if access::is_atom(term) {
        atoms.name(atom_val(term))
    } else {
        None
    }
}

/// The `int` value of a small integer, as the C `term_to_int` requires.
fn small_int(term: Eterm) -> Option<i64> {
    if access::is_small(term) {
        Some(signed_value(term))
    } else {
        None
    }
}

/// The byte payload of an iodata term, or `None` when it is not valid iodata
/// (the C `erts_iolist_size`/`erts_iolist_to_buf` pair).
fn iodata_bytes(term: Eterm) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    fn walk(term: Eterm, out: &mut Vec<u8>) -> Option<()> {
        if access::is_nil(term) {
            return Some(());
        }
        if access::is_list(term) {
            walk(access::car(term), out)?;
            return walk(access::cdr(term), out);
        }
        // A list *element* is a byte (`iolist() = binary() | [byte() | binary() |
        // iolist()]`), so a name written as a string — `["foo"]`, which Erlang
        // spells as a list of small integers — is valid iodata.
        if let Some(byte) = byte_value(term) {
            out.push(byte);
            return Some(());
        }
        if access::is_binary(term) {
            out.extend_from_slice(&crate::bits::bytes(term).ok()?);
            return Some(());
        }
        None
    }
    walk(term, &mut out)?;
    Some(out)
}

/// A small integer in `0..=255` as a byte (the `erts_iolist_to_buf` element rule).
fn byte_value(term: Eterm) -> Option<u8> {
    u8::try_from(small_int(term)?).ok()
}

/// One option list element.
fn scan_item(atoms: &AtomTable, item: Eterm, out: &mut Parsed) -> Result<()> {
    if access::is_tuple(item) {
        let arity = access::tuple_size(item);
        let tag = if arity >= 1 { access::tuple_element(item, 0) } else { return Err(badarg()) };
        // `{capture, Values, Type}` — the only 3-tuple the oracle accepts.
        if arity == 3 {
            if atom_name(atoms, tag) != Some("capture") {
                return Err(badarg());
            }
            let value = capture_value(atoms, access::tuple_element(item, 1))?;
            let ty = capture_type(atoms, access::tuple_element(item, 2))?;
            out.capture = Some((value, ty));
            out.unique_exec = true;
            return Ok(());
        }
        if arity != 2 || !access::is_atom(tag) {
            return Err(badarg());
        }
        return scan_pair(atoms, atom_name(atoms, tag).unwrap_or(""), item, out);
    }
    if access::is_atom(item) {
        let name = atom_name(atoms, item).unwrap_or("");
        return scan_flag(name, out);
    }
    Err(badarg())
}

/// A `{Tag, Value}` option tuple.
fn scan_pair(atoms: &AtomTable, tag: &str, item: Eterm, out: &mut Parsed) -> Result<()> {
    let value = access::tuple_element(item, 1);
    match tag {
        "capture" => {
            out.capture = Some((capture_value(atoms, value)?, CapType::Index));
            out.unique_exec = true;
        }
        "offset" => {
            let n = small_int(value).filter(|&n| n >= 0).ok_or_else(badarg)?;
            out.start_offset = u32::try_from(n).map_err(|_| badarg())?;
            out.unique_exec = true;
        }
        "match_limit" => {
            let n = small_int(value).filter(|&n| n >= 0).ok_or_else(badarg)?;
            out.match_limit = Some(u32::try_from(n).map_err(|_| badarg())?);
            out.unique_exec = true;
        }
        "match_limit_recursion" => {
            let n = small_int(value).filter(|&n| n >= 0).ok_or_else(badarg)?;
            out.match_limit_recursion = Some(u32::try_from(n).map_err(|_| badarg())?);
            out.unique_exec = true;
        }
        "newline" => {
            let nl = match atom_name(atoms, value) {
                Some("cr") => Newline::Cr,
                Some("crlf") => Newline::Crlf,
                Some("lf") => Newline::Lf,
                Some("nul") => Newline::Nul,
                Some("anycrlf") => Newline::AnyCrlf,
                Some("any") => Newline::Any,
                _ => return Err(badarg()),
            };
            out.newline_opt = Some(nl);
            out.compile.newline = nl;
        }
        _ => return Err(badarg()),
    }
    Ok(())
}

/// A bare option atom.
fn scan_flag(name: &str, out: &mut Parsed) -> Result<()> {
    match name {
        "anchored" => out.compile.anchored = true,
        "notempty" => {
            out.notempty = true;
            out.unique_exec = true;
        }
        "notempty_atstart" => {
            out.notempty_atstart = true;
            out.unique_exec = true;
        }
        "notbol" => {
            out.notbol = true;
            out.unique_exec = true;
        }
        "noteol" => {
            out.noteol = true;
            out.unique_exec = true;
        }
        "report_errors" => {
            out.report_errors = true;
            out.unique_exec = true;
        }
        "global" => {
            out.global = true;
            out.unique_exec = true;
        }
        "no_start_optimize" => {
            out.compile.no_start_optimize = true;
            out.unique_compile = true;
        }
        "caseless" => {
            out.compile.caseless = true;
            out.unique_compile = true;
        }
        "dollar_endonly" => {
            out.compile.dollar_endonly = true;
            out.unique_compile = true;
        }
        "dotall" => {
            out.compile.dotall = true;
            out.unique_compile = true;
        }
        "extended" => {
            out.compile.extended = true;
            out.unique_compile = true;
        }
        "firstline" => {
            out.compile.firstline = true;
            out.unique_compile = true;
        }
        "multiline" => {
            out.compile.multiline = true;
            out.unique_compile = true;
        }
        "no_auto_capture" => {
            out.compile.no_auto_capture = true;
            out.unique_compile = true;
        }
        "dupnames" => {
            out.compile.dupnames = true;
            out.unique_compile = true;
        }
        "ungreedy" => {
            out.compile.ungreedy = true;
            out.unique_compile = true;
        }
        "ucp" => {
            out.compile.ucp = true;
            out.unique_compile = true;
        }
        "never_utf" => {
            out.compile.never_utf = true;
            out.unique_compile = true;
        }
        "unicode" => {
            out.compile.unicode = true;
            out.unique_compile = true;
        }
        "bsr_anycrlf" => out.bsr_opt = Some(Bsr::AnyCrlf),
        "bsr_unicode" => out.bsr_opt = Some(Bsr::Unicode),
        "export" => {
            out.export = true;
            out.unique_compile = true;
        }
        _ => return Err(badarg()),
    }
    Ok(())
}

/// The `ValueSpec` of a capture specification.
fn capture_value(atoms: &AtomTable, value: Eterm) -> Result<CapValue> {
    if access::is_nil(value) {
        return Ok(CapValue::None);
    }
    if access::is_atom(value) {
        return match atom_name(atoms, value) {
            Some("all") => Ok(CapValue::All),
            Some("all_but_first") => Ok(CapValue::AllButFirst),
            Some("all_names") => Ok(CapValue::AllNames),
            Some("first") => Ok(CapValue::First),
            Some("none") => Ok(CapValue::None),
            _ => Err(badarg()),
        };
    }
    if !access::is_list(value) {
        return Err(badarg());
    }
    let mut ids = Vec::new();
    let mut cur = value;
    while access::is_list(cur) {
        let elem = access::car(cur);
        if let Some(n) = small_int(elem) {
            let n = u32::try_from(n).map_err(|_| badarg())?;
            ids.push(CapId::Index(n));
        } else if let Some(name) = atom_name(atoms, elem) {
            ids.push(CapId::Name(name.to_string()));
        } else if access::is_bitstring(elem) || access::is_list(elem) {
            let bytes = iodata_bytes(elem).ok_or_else(badarg)?;
            ids.push(CapId::Name(String::from_utf8_lossy(&bytes).into_owned()));
        } else {
            return Err(badarg());
        }
        cur = access::cdr(cur);
    }
    if !access::is_nil(cur) {
        return Err(badarg());
    }
    Ok(CapValue::Names(ids))
}

/// The `Type` of a capture specification.
fn capture_type(atoms: &AtomTable, value: Eterm) -> Result<CapType> {
    match atom_name(atoms, value) {
        Some("index") => Ok(CapType::Index),
        Some("list") => Ok(CapType::List),
        Some("binary") => Ok(CapType::Binary),
        _ => Err(badarg()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::{make_atom, make_small, NIL};

    /// Builds small terms by hand on a throw-away heap.
    struct Fixture {
        heap: Heap,
        atoms: AtomTable,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut atoms = AtomTable::new();
            atoms.seed([
                "capture", "all", "all_but_first", "all_names", "first", "none", "index", "list",
                "binary", "offset", "match_limit", "match_limit_recursion", "newline", "cr", "crlf",
                "lf", "nul", "anycrlf", "any", "anchored", "notempty", "notempty_atstart", "notbol",
                "noteol", "report_errors", "global", "export", "unicode", "caseless", "dupnames",
                "bogus",
            ]);
            Fixture { heap: Heap::new(64).expect("test heap"), atoms }
        }

        fn atom(&self, name: &str) -> Eterm {
            make_atom(self.atoms.lookup(name).expect("seeded"))
        }

        fn tup(&mut self, elems: &[Eterm]) -> Eterm {
            self.heap.tuple(elems).expect("the tuple fits")
        }

        /// Builds a proper list of `elems` (right to left, on the fixture heap).
        fn list(&mut self, elems: &[Eterm]) -> Eterm {
            let mut tail = NIL;
            for &elem in elems.iter().rev() {
                tail = self.heap.cons(elem, tail).expect("the list fits");
            }
            tail
        }

        /// Builds the option list from its elements and parses it.
        ///
        /// The single entry point the tests use: it resolves the `&self.atoms`
        /// / `&mut self.heap` borrows inside one call, which the borrow checker
        /// rejects when a test spells them out in the same expression.
        fn parse_opts(&mut self, elems: &[Eterm]) -> beam_common::Result<Parsed> {
            let list = self.list(elems);
            parse(&self.atoms, list)
        }
    }
    #[test]
    fn bare_flags_are_classified() {
        let mut f = Fixture::new();
        let opts = f.list(&[f.atom("caseless"), f.atom("global")]);
        let parsed = parse(&f.atoms, opts).expect("a valid list");
        assert!(parsed.compile.caseless);
        assert!(parsed.unique_compile);
        assert!(parsed.global);
        assert!(parsed.unique_exec);
        assert!(!parsed.compile.unicode);
    }

    #[test]
    fn unknown_options_are_badarg() {
        let mut f = Fixture::new();
        let opts = f.list(&[f.atom("bogus")]);
        assert!(matches!(parse(&f.atoms, opts), Err(beam_common::BeamError::BadArg)));
        // An improper list is rejected too.
        let global = f.atom("global");
        let improper = f.heap.cons(global, make_small(0)).expect("fits");
        assert!(matches!(parse(&f.atoms, improper), Err(beam_common::BeamError::BadArg)));
        // ... and a non-list.
        assert!(matches!(parse(&f.atoms, f.atom("global")), Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn capture_specs_parse_into_their_shape() {
        let mut f = Fixture::new();
        let cap = f.atom("capture");
        let all = f.atom("all");
        let index = f.atom("index");
        let list = f.atom("list");
        let none = f.atom("none");

        let two = f.tup(&[cap, all]);
        let p = f.parse_opts(&[two]).expect("capture/all");
        assert_eq!(p.capture, Some((CapValue::All, CapType::Index)));

        let three = f.tup(&[cap, all, list]);
        let p = f.parse_opts(&[three]).expect("capture/all/list");
        assert_eq!(p.capture, Some((CapValue::All, CapType::List)));

        let none_spec = f.tup(&[cap, none]);
        let p = f.parse_opts(&[none_spec]).expect("capture/none");
        assert_eq!(p.capture, Some((CapValue::None, CapType::Index)));

        // The empty list is `none`.
        let empty = f.tup(&[cap, NIL]);
        assert_eq!(NIL, f.list(&[]));
        let p = f.parse_opts(&[empty]).expect("capture/[]");
        assert_eq!(p.capture, Some((CapValue::None, CapType::Index)));

        // An explicit id list keeps its order, and a negative id is badarg.
        let ids = f.list(&[make_small(2), make_small(1)]);
        let listed = f.tup(&[cap, ids, index]);
        let p = f.parse_opts(&[listed]).expect("capture/[2,1]");
        assert_eq!(p.capture, Some((CapValue::Names(vec![CapId::Index(2), CapId::Index(1)]), CapType::Index)));

        let neg_list = f.list(&[make_small(-1)]);
        let bad = f.tup(&[cap, neg_list, index]);
        assert!(matches!(f.parse_opts(&[bad]), Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn offsets_and_limits_reject_negative_values() {
        let mut f = Fixture::new();
        let offset = f.atom("offset");
        let spec = f.tup(&[offset, make_small(5)]);
        let p = f.parse_opts(&[spec]).expect("{offset,5}");
        assert_eq!(p.start_offset, 5);

        let bad = f.tup(&[offset, make_small(-1)]);
        assert!(matches!(f.parse_opts(&[bad]), Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn newline_specs_map_to_the_conventions() {
        let mut f = Fixture::new();
        for (name, want) in [
            ("cr", Newline::Cr),
            ("crlf", Newline::Crlf),
            ("lf", Newline::Lf),
            ("nul", Newline::Nul),
            ("anycrlf", Newline::AnyCrlf),
            ("any", Newline::Any),
        ] {
            let nl = f.atom("newline");
            let value = f.atom(name);
            let spec = f.tup(&[nl, value]);
            let p = f.parse_opts(&[spec]).expect("a valid newline spec");
            assert_eq!(p.newline_opt, Some(want));
            assert_eq!(p.compile.newline, want);
        }
        assert!(Newline::Crlf.is_crlf_like());
        assert!(Newline::AnyCrlf.is_crlf_like());
        assert!(Newline::Any.is_crlf_like());
        assert!(!Newline::Cr.is_crlf_like());
        assert!(!Newline::Lf.is_crlf_like());

        let nl = f.atom("newline");
        let value = f.atom("bogus");
        let spec = f.tup(&[nl, value]);
        assert!(matches!(f.parse_opts(&[spec]), Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn newline_matching_rules() {
        let lf = |s: &[u32], p: usize| Newline::Lf.newline_len_at(s, p);
        assert_eq!(lf(&[10], 0), Some(1));
        assert_eq!(lf(&[13], 0), None);
        let crlf = |s: &[u32], p: usize| Newline::Crlf.newline_len_at(s, p);
        assert_eq!(crlf(&[13, 10], 0), Some(2));
        assert_eq!(crlf(&[10], 0), Some(1));
        assert_eq!(crlf(&[13], 0), Some(1));
        let any = |s: &[u32], p: usize| Newline::Any.newline_len_at(s, p);
        assert_eq!(any(&[0x2028], 0), Some(1));
        assert_eq!(any(&[0x85], 0), Some(1));
        assert_eq!(Newline::AnyCrlf.newline_len_at(&[0x2028], 0), None);

        // `ends_at` for the CRLF convention accepts a lone `\n` / `\r`.
        assert!(Newline::Crlf.ends_at(&[13, 10, 98], 2));
        assert!(Newline::Crlf.ends_at(&[10, 98], 1));
        assert!(!Newline::Crlf.ends_at(&[97, 98], 1));

        // `.` excludes both bytes of a CRLF pair under the CRLF convention.
        assert!(Newline::Crlf.is_newline_char(13));
        assert!(Newline::Crlf.is_newline_char(10));
        assert!(!Newline::Cr.is_newline_char(10));
        assert!(Newline::Any.is_newline_char(0x2029));
    }
}
