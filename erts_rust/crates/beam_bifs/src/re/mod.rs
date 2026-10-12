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

//! The in-crate regular-expression engine serving the `re:*` BIFs **for real**.
//!
//! This node implements the 8 declared `bif.tab` `re` keys (`version/0`,
//! `compile/1,2`, `run/2,3`, `internal_run/4`, `inspect/2`, `import/1`) with a
//! self-contained engine written from scratch in safe Rust — **no third-party
//! regex engine, no PCRE** (workspace licensing is Apache-2.0/MIT only). The
//! design contract (compiled-pattern term shape, capture semantics, option set,
//! error behaviour and the oracle goldens) lives in this node's `CONTEXT.md`.
//!
//! `re:compile/1,2` are implemented: the option list is parsed, the pattern is
//! lowered to the self-contained payload image and the oracle's
//! `{ok, {re_pattern, NCap, UnicodeFlag, CrlfFlag, Payload}}` (or, with the
//! `export` option, the `{re_exported_pattern, HeaderBin, Pattern, Options,
//! Payload}` form) is returned. The remaining six keys still answer the typed
//! `unsupported_bif` capability gap; the engine implementation fills those bodies
//! in (and adds the sibling `exec`/`render` submodules) in a later stage.

use beam_common::{BeamError, Result};
use beam_term::{make_small, signed_value, Eterm, NIL};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

// Temporary: the run-side consumers of these modules (the `exec`/`render`
// siblings adopted by `re:run/2,3`, `re:internal_run/4` and `re:inspect/2`) are
// still placeholders, so the match-side half of each module is unreachable from
// non-test code. The allow goes when those bodies land.
#[allow(dead_code)]
mod options;
// Temporary: see `options` above — `chars_equal`/`fold_key`/`E_POSIX_OUTSIDE`
// are match-side and only exercised by the unit tests today.
#[allow(dead_code)]
mod syntax;
// Temporary: see `options` above — `Payload::decode` and the image reader are
// only reached from `re:import/1`, which is still a placeholder.
#[allow(dead_code)]
mod prog;

/// The compiled-pattern tag of the plain `{re_pattern, …}` form.
const RE_PATTERN: &str = "re_pattern";

/// The compiled-pattern tag of the `export` form.
const RE_EXPORTED_PATTERN: &str = "re_exported_pattern";

/// The 8 `(module, function, arity)` keys this node implements **for real** (not
/// through the `RuntimeHooks` seam).
///
/// Sorted by `(module, function, arity)`. `src/runtime/other.rs` aliases this list
/// as its own `REAL_BIFS`, so the roster has a single source.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("re", "compile", 1),
    ("re", "compile", 2),
    ("re", "import", 1),
    ("re", "inspect", 2),
    ("re", "internal_run", 4),
    ("re", "run", 2),
    ("re", "run", 3),
    ("re", "version", 0),
];

/// `re:compile/1` — the same as `re:compile/2` with an empty option list.
fn compile_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    compile(ctx, args[0], NIL)
}

/// `re:compile/2` — compile a pattern into the `{re_pattern, …}` term (or, with the
/// `export` option, the `{re_exported_pattern, …}` one).
fn compile_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    compile(ctx, args[0], args[1])
}

/// The shared `re:compile/1,2` body (`re_compile`, `erl_bif_re.c:890-955`).
///
/// # Errors
///
/// A **raise** for every bad *argument* ([`BeamError::BadArg`]: a non-iodata
/// pattern, an out-of-range element, an unknown/run-only option) and for a
/// construct outside the supported subset (the typed `unsupported_bif`). A
/// **malformed pattern** is a returned `{error, {Msg, Pos}}` *value*, never a
/// raise — including its exact PCRE2 message and 0-based byte position.
fn compile(ctx: &mut BifContext<'_>, pattern: Eterm, options: Eterm) -> Result<Eterm> {
    let parsed = options::parse(ctx.atoms(), options)?;
    if parsed.unique_exec {
        // A run-only option (`global`, `{capture,…}`, `{offset,…}`, …) — the
        // oracle's `PARSE_FLAG_UNIQUE_EXEC_OPT` refusal.
        return Err(badarg());
    }
    let flags = &parsed.compile;
    let (codes, offs, source) = pattern_form(pattern, flags.unicode)?;
    let syntax = match syntax::parse(&codes, &offs, source.len(), flags) {
        Ok(syntax) => syntax,
        Err(err) => return compile_error(ctx, err),
    };
    let program = prog::compile(&syntax, flags)?;
    let payload = prog::Payload { program, source };
    let image = payload.encode();
    let image_bin = bits::make_binary(ctx.heap(), &image)?;
    let ok = ctx.intern_atom("ok")?;
    if parsed.export {
        let header = bits::make_binary(ctx.heap(), prog::MAGIC)?;
        let orig = bits::make_binary(ctx.heap(), &payload.source)?;
        let tag = ctx.intern_atom(RE_EXPORTED_PATTERN)?;
        let exported = ctx.tuple(&[tag, header, orig, options, image_bin])?;
        return ctx.tuple(&[ok, exported]);
    }
    let tag = ctx.intern_atom(RE_PATTERN)?;
    let ncap = ctx.int(i64::from(payload.program.ncap))?;
    let unicode = ctx.int(i64::from(flags.unicode))?;
    let crlf = ctx.int(i64::from(flags.newline.is_crlf_like()))?;
    let mp = ctx.tuple(&[tag, ncap, unicode, crlf, image_bin])?;
    ctx.tuple(&[ok, mp])
}

/// Renders a compile failure as the `{error, {Msg, Pos}}` **value** the oracle
/// returns.
///
/// The [`syntax::ParseError`] travels out of `syntax::parse` in the raise's opaque
/// live-reason slot (see `syntax.rs`'s `From<ParseError> for BeamError`); a raise
/// that carries no `ParseError` is the typed `unsupported_bif` capability gap and
/// is propagated unchanged.
fn compile_error(ctx: &mut BifContext<'_>, err: BeamError) -> Result<Eterm> {
    let parse_error = match &err {
        BeamError::Raise(raise) => raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<syntax::ParseError>())
            .copied(),
        _ => None,
    };
    let Some(e) = parse_error else {
        return Err(err);
    };
    // `Msg` is a charlist of small ints (the PCRE2 message is ASCII).
    let chars: Vec<Eterm> = e.msg.bytes().map(|b| make_small(i64::from(b))).collect();
    let msg = ctx.list(&chars)?;
    let pos = ctx.int(i64::try_from(e.pos).unwrap_or(i64::MAX))?;
    let pair = ctx.tuple(&[msg, pos])?;
    let tag = ctx.intern_atom("error")?;
    ctx.tuple(&[tag, pair])
}

/// The pattern's byte image plus its codepoint form.
///
/// Returns `(codes, offs, source)`: `codes[i]` is the pattern's `i`th codepoint,
/// `offs[i]` its byte offset in `source` and `source` the pattern's raw byte image
/// (what the `export` form returns as the original pattern). That is exactly the
/// triple [`syntax::parse`] wants, so an end-of-pattern error reports the offset
/// just past the last byte.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata pattern, an out-of-range element, a
/// non-byte-aligned bitstring, and (under `unicode`) a pattern that is not valid
/// UTF-8 — the oracle's `get_iolist_as_bytes` / `unicode:characters_to_binary/2`
/// failures.
fn pattern_form(pattern: Eterm, unicode: bool) -> Result<(Vec<u32>, Vec<usize>, Vec<u8>)> {
    // Only a bitstring and a list are iodata; a bare integer (or any other term) is
    // the oracle's `badarg` (`re:compile(123)`).
    if !bits::is_bitstring(pattern) && !access::is_list(pattern) && !access::is_nil(pattern) {
        return Err(badarg());
    }
    let mut source: Vec<u8> = Vec::new();
    if bits::is_bitstring(pattern) {
        source = bits::bytes(pattern)?;
    } else {
        flatten(pattern, unicode, &mut source)?;
    }
    if !unicode {
        // One codepoint per byte (the Latin-1 view PCRE2 compiles without UTF).
        let codes: Vec<u32> = source.iter().map(|&b| u32::from(b)).collect();
        let offs: Vec<usize> = (0..source.len()).collect();
        return Ok((codes, offs, source));
    }
    let text = std::str::from_utf8(&source).map_err(|_| badarg())?;
    let codes: Vec<u32> = text.chars().map(u32::from).collect();
    let offs: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    Ok((codes, offs, source))
}

/// Flattens an iodata pattern into its byte image.
///
/// The walk is **iterative** (an explicit work stack), so a deeply nested pattern
/// cannot exhaust the native stack. Under `unicode` the bytes are the UTF-8
/// encoding of a `unicode:chardata()` (the `ucompile` trap's
/// `unicode:characters_to_binary(Pattern, unicode)`); otherwise every integer is
/// one byte (`erts_iolist_to_buf`). An improper list is valid iodata only when its
/// tail is a binary.
///
/// # Errors
///
/// [`BeamError::BadArg`] for an element outside `0..=255` (or an invalid codepoint
/// under `unicode`), a non-byte-aligned bitstring, and any term that is not iodata.
fn flatten(pattern: Eterm, unicode: bool, out: &mut Vec<u8>) -> Result<()> {
    let mut stack: Vec<Eterm> = vec![pattern];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_list(term) {
            // The tail is in "iodata tail" position (a list, nil or a binary).
            stack.push(access::cdr(term));
            push_element(access::car(term), unicode, out, &mut stack)?;
            continue;
        }
        if bits::is_bitstring(term) {
            out.extend_from_slice(&bits::bytes(term)?);
            continue;
        }
        // An integer in tail position is not iodata.
        return Err(badarg());
    }
    Ok(())
}

/// Handles one list *element* (which, unlike a tail, may be an integer).
fn push_element(
    term: Eterm,
    unicode: bool,
    out: &mut Vec<u8>,
    stack: &mut Vec<Eterm>,
) -> Result<()> {
    if access::is_list(term) {
        stack.push(term);
        return Ok(());
    }
    if bits::is_bitstring(term) {
        out.extend_from_slice(&bits::bytes(term)?);
        return Ok(());
    }
    if !access::is_small(term) {
        return Err(badarg());
    }
    let n = signed_value(term);
    if unicode {
        let c = u32::try_from(n).ok().and_then(char::from_u32).ok_or_else(badarg)?;
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    } else {
        out.push(u8::try_from(n).map_err(|_| badarg())?);
    }
    Ok(())
}

/// `re:import/1` — the placeholder the engine implementation replaces.
fn import_1(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:inspect/2` — the placeholder the engine implementation replaces.
fn inspect_2(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:internal_run/4` — the placeholder the engine implementation replaces.
fn internal_run_4(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:run/2` — the placeholder the engine implementation replaces.
fn run_2(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// `re:run/3` — the placeholder the engine implementation replaces.
fn run_3(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(crate::error::unsupported_bif())
}

/// The `re` surface version string this engine models.
///
/// This is the **compatibility-target version string of the `re` surface we
/// model** — the value the reference oracle (`OTP 29 / erts-17.1`) reports for
/// `re:version/0`. It is *not* an assertion that PCRE2 is present: this crate
/// contains no PCRE, only a self-contained engine that implements a subset of
/// the same pattern language. The string is pinned so a caller that gates
/// behaviour on the reported `re` version sees the value it would see on the
/// reference runtime.
pub(crate) const RE_VERSION: &[u8] = b"10.47 2025-10-21";

/// `re:version/0` — the modelled `re` surface version, as a binary.
fn version_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    crate::bits::make_binary(ctx.heap(), RE_VERSION)
}

/// Registers the 8 real `re:*` BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("re", "compile", 1, compile_1);
    reg.register("re", "compile", 2, compile_2);
    reg.register("re", "import", 1, import_1);
    reg.register("re", "inspect", 2, inspect_2);
    reg.register("re", "internal_run", 4, internal_run_4);
    reg.register("re", "run", 2, run_2);
    reg.register("re", "run", 3, run_3);
    reg.register("re", "version", 0, version_0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{compare, make_atom, AtomTable};

    /// A registry + heap + seeded atom table the tests drive `re:compile` through.
    struct Fixture {
        registry: BifRegistry,
        heap: Heap,
        atoms: AtomTable,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut atoms = AtomTable::new();
            atoms.seed([
                "ok",
                "error",
                "re_pattern",
                "re_exported_pattern",
                "unicode",
                "newline",
                "cr",
                "crlf",
                "lf",
                "nul",
                "anycrlf",
                "any",
                "export",
                "global",
                "capture",
                "all",
                "index",
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

        fn call(&mut self, arity: u32, args: &[Eterm]) -> Result<Eterm> {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            self.registry.call("re", "compile", arity, &mut ctx, args)
        }

        /// `re:compile/2` on a binary pattern and a slice of option terms.
        fn compile_bin(&mut self, pattern: &[u8], opts: &[Eterm]) -> Result<Eterm> {
            let pattern = self.binary(pattern);
            let opts = self.list(opts);
            self.call(2, &[pattern, opts])
        }

        /// `re:compile/2` on a binary pattern and an already-built option list.
        ///
        /// The list term is passed through by value, so a test that spells the
        /// option list out does not need two borrows of the fixture at once.
        fn compile_with(&mut self, pattern: &[u8], opts: Eterm) -> Result<Eterm> {
            let pattern = self.binary(pattern);
            self.call(2, &[pattern, opts])
        }
        fn eq(&self, a: Eterm, b: Eterm) -> bool {
            compare::eq(a, b, &self.atoms)
        }

        fn elem(&self, tuple: Eterm, i: usize) -> Eterm {
            access::tuple_element(tuple, i)
        }

        fn arity(&self, tuple: Eterm) -> usize {
            access::tuple_size(tuple)
        }

        fn small(&self, term: Eterm) -> i64 {
            assert!(access::is_small(term), "expected a small int");
            signed_value(term)
        }

        /// The payload image of a compiled `{re_pattern, …}` term.
        fn image(&self, mp: Eterm) -> Vec<u8> {
            bits::bytes(self.elem(mp, 4)).expect("the payload is a byte-aligned binary")
        }
    }

    #[test]
    fn compile_returns_the_five_element_re_pattern_shape() {
        let mut f = Fixture::new();
        let out = f.compile_bin(b"(a)(b)", &[]).expect("compiles");
        assert_eq!(f.arity(out), 2);
        assert!(f.eq(f.elem(out, 0), f.atom("ok")));
        let mp = f.elem(out, 1);
        assert_eq!(f.arity(mp), 5);
        assert!(f.eq(f.elem(mp, 0), f.atom("re_pattern")));
        assert_eq!(f.small(f.elem(mp, 1)), 2, "NCap excludes the whole match");
        assert_eq!(f.small(f.elem(mp, 2)), 0, "no unicode");
        assert_eq!(f.small(f.elem(mp, 3)), 0, "newline lf");
        // The payload is our own image, never the oracle's `#Ref<…>`.
        let decoded = prog::Payload::decode(&f.image(mp)).expect("our own image decodes");
        assert_eq!(decoded.program.ncap, 2);
    }

    #[test]
    fn compile_one_matches_compile_two_with_an_empty_option_list() {
        let mut f = Fixture::new();
        let one = f.binary(b"(a)(b)");
        let out1 = f.call(1, &[one]).expect("compiles");
        let two = f.binary(b"(a)(b)");
        let empty = f.list(&[]);
        let out2 = f.call(2, &[two, empty]).expect("compiles");
        assert!(f.eq(out1, out2));
    }

    #[test]
    fn ncap_counts_capturing_groups_only() {
        let mut f = Fixture::new();
        for (pattern, ncap) in [
            (&b""[..], 0i64),
            (b"a", 0),
            (b"(a)", 1),
            (b"(a)(b)", 2),
            (b"(a)(?:b)(c)", 2),
            (b"(?<n>a)", 1),
            (b"(a)|(b)|(c)", 3),
        ] {
            let out = f.compile_bin(pattern, &[]).expect("compiles");
            let mp = f.elem(out, 1);
            assert_eq!(f.small(f.elem(mp, 1)), ncap, "{pattern:?}");
        }
        // The empty charlist is the empty pattern (group 0 is not counted).
        let out = f.call(1, &[NIL]).expect("the empty pattern compiles");
        assert_eq!(f.small(f.elem(f.elem(out, 1), 1)), 0);
        let decoded = prog::Payload::decode(&f.image(f.elem(out, 1))).expect("decodes");
        assert!(decoded.source.is_empty());
    }

    #[test]
    fn unicode_encodes_a_charlist_and_sets_the_pattern_flag() {
        let mut f = Fixture::new();
        let unicode = f.atom("unicode");
        let opts = f.list(&[unicode]);

        // A binary pattern: the bytes are already UTF-8.
        let pattern = f.binary("å".as_bytes());
        let out = f.call(2, &[pattern, opts]).expect("compiles");
        let mp = f.elem(out, 1);
        assert_eq!(f.small(f.elem(mp, 2)), 1, "the unicode flag is element 3");
        let decoded = prog::Payload::decode(&f.image(mp)).expect("decodes");
        assert_eq!(decoded.source.as_slice(), "å".as_bytes());
        assert_eq!(decoded.program.ncap, 0);

        // A charlist: the codepoint is encoded back to UTF-8, so the payload's
        // source is the same two bytes.
        let pattern = f.charlist("å");
        let out = f.call(2, &[pattern, opts]).expect("compiles");
        let mp = f.elem(out, 1);
        assert_eq!(f.small(f.elem(mp, 2)), 1);
        assert_eq!(prog::Payload::decode(&f.image(mp)).expect("decodes").source.as_slice(), "å".as_bytes());

        // Without `unicode` the flag is 0 and a charlist is Latin-1.
        let pattern = f.charlist("a");
        let out = f.call(1, &[pattern]).expect("compiles");
        assert_eq!(f.small(f.elem(f.elem(out, 1), 2)), 0);
    }

    #[test]
    fn the_newline_convention_sets_the_crlf_flag() {
        let mut f = Fixture::new();
        for (spec, expected) in
            [("crlf", 1i64), ("anycrlf", 1), ("any", 1), ("lf", 0), ("cr", 0), ("nul", 0)]
        {
            let newline = f.atom("newline");
            let value = f.atom(spec);
            let pair = f.tuple(&[newline, value]);
            let opts = f.list(&[pair]);
            let out = f.compile_with(b"a", opts).expect("compiles");
            let mp = f.elem(out, 1);
            assert_eq!(f.small(f.elem(mp, 3)), expected, "{spec}");
        }
    }

    #[test]
    fn a_malformed_pattern_returns_the_oracle_error_tuple() {
        let mut f = Fixture::new();
        for (pattern, msg, pos) in [
            (&b"a(b"[..], "missing closing parenthesis", 3i64),
            (b"[a", "missing terminating ] for character class", 2),
            (b"a{2,1}", "numbers out of order in {} quantifier", 5),
            (b"\\", "\\ at end of pattern", 1),
        ] {
            let out = f.compile_bin(pattern, &[]).expect("a compile error is a value, not a raise");
            assert_eq!(f.arity(out), 2, "{pattern:?}");
            assert!(f.eq(f.elem(out, 0), f.atom("error")), "{pattern:?}");
            let pair = f.elem(out, 1);
            assert_eq!(f.arity(pair), 2, "{pattern:?}");
            let chars: Vec<Eterm> =
                msg.bytes().map(|b| make_small(i64::from(b))).collect();
            let expected = f.list(&chars);
            assert!(f.eq(f.elem(pair, 0), expected), "{pattern:?} message");
            assert_eq!(f.small(f.elem(pair, 1)), pos, "{pattern:?} position");
            assert!(access::is_list(f.elem(pair, 0)), "the message is a charlist");
        }
    }

    #[test]
    fn a_non_iodata_pattern_raises_badarg() {
        let mut f = Fixture::new();
        let opts = f.list(&[]);
        // A bare integer (`re:compile(123)`).
        let int = make_small(123);
        assert!(matches!(f.call(1, &[int]), Err(BeamError::BadArg)));
        assert!(matches!(f.call(2, &[int, opts]), Err(BeamError::BadArg)));
        // An atom is not iodata either.
        let nope = f.atom("nope");
        assert!(matches!(f.call(1, &[nope]), Err(BeamError::BadArg)));
        // An improper list whose tail is not a binary.
        let improper = f.heap.cons(make_small(97), make_small(0)).expect("fits");
        assert!(matches!(f.call(1, &[improper]), Err(BeamError::BadArg)));
        // An element above 255 without `unicode`.
        let big = f.list(&[make_small(256)]);
        assert!(matches!(f.call(1, &[big]), Err(BeamError::BadArg)));
        // A negative element.
        let negative = f.list(&[make_small(-1)]);
        assert!(matches!(f.call(1, &[negative]), Err(BeamError::BadArg)));
        // ... and a non-integer element.
        let atom_elem = f.list(&[f.atom("nope")]);
        assert!(matches!(f.call(1, &[atom_elem]), Err(BeamError::BadArg)));
    }

    #[test]
    fn iodata_nesting_and_a_binary_tail_are_accepted() {
        let mut f = Fixture::new();
        // A binary element and a nested list — the oracle's `iodata()`.
        let a = f.binary(b"a");
        let b = f.binary(b"b");
        let inner = f.list(&[make_small(i64::from(b'c'))]);
        let nested = f.list(&[a, inner, b]);
        let out = f.call(1, &[nested]).expect("iodata compiles");
        let mp = f.elem(out, 1);
        assert_eq!(prog::Payload::decode(&f.image(mp)).expect("decodes").source.as_slice(), b"acb");

        // An improper list whose tail is a binary is iodata.
        let head = make_small(i64::from(b'a'));
        let tail = f.binary(b"b");
        let improper = f.heap.cons(head, tail).expect("fits");
        let out = f.call(1, &[improper]).expect("an improper iodata list compiles");
        assert_eq!(prog::Payload::decode(&f.image(f.elem(out, 1))).expect("decodes").source.as_slice(), b"ab");
    }

    #[test]
    fn a_run_only_option_raises_badarg() {
        let mut f = Fixture::new();
        let global = f.atom("global");
        let opts = f.list(&[global]);
        assert!(matches!(f.compile_with(b"a", opts), Err(BeamError::BadArg)));

        let capture = f.atom("capture");
        let all = f.atom("all");
        let index = f.atom("index");
        let capture_opt = f.tuple(&[capture, all, index]);
        let opts = f.list(&[capture_opt]);
        assert!(matches!(f.compile_with(b"a", opts), Err(BeamError::BadArg)));

        // An unknown option.
        let nope = f.atom("nope");
        let opts = f.list(&[nope]);
        assert!(matches!(f.compile_with(b"a", opts), Err(BeamError::BadArg)));

        // A compile-only option is accepted.
        let unicode = f.atom("unicode");
        let opts = f.list(&[unicode]);
        assert!(f.compile_with(b"a", opts).is_ok());
    }

    #[test]
    fn export_returns_the_exported_pattern_shape() {
        let mut f = Fixture::new();
        let export = f.atom("export");
        let opts = f.list(&[export]);
        let out = f.compile_with(b"a(b)", opts).expect("compiles");
        assert!(f.eq(f.elem(out, 0), f.atom("ok")));
        let exported = f.elem(out, 1);
        assert_eq!(f.arity(exported), 5);
        assert!(f.eq(f.elem(exported, 0), f.atom("re_exported_pattern")));
        assert_eq!(bits::bytes(f.elem(exported, 1)).expect("header"), prog::MAGIC.to_vec());
        assert_eq!(bits::bytes(f.elem(exported, 2)).expect("pattern").as_slice(), b"a(b)");
        assert!(f.eq(f.elem(exported, 3), opts), "the option list is returned as given");
        let decoded =
            prog::Payload::decode(&bits::bytes(f.elem(exported, 4)).expect("payload")).expect("decodes");
        assert_eq!(decoded.source.as_slice(), b"a(b)");
        assert_eq!(decoded.program.ncap, 1);
    }

    #[test]
    fn the_payload_image_round_trips_to_the_same_program_as_the_ast() {
        let mut f = Fixture::new();
        let out = f.compile_bin(b"(a)(b)|c", &[]).expect("compiles");
        let decoded = prog::Payload::decode(&f.image(f.elem(out, 1))).expect("decodes");
        assert_eq!(decoded.source.as_slice(), b"(a)(b)|c");

        // The decoded program is the one the parsed AST compiles to.
        let flags = options::CompileFlags::default();
        let codes: Vec<u32> = b"(a)(b)|c".iter().map(|&b| u32::from(b)).collect();
        let offs: Vec<usize> = (0..codes.len()).collect();
        let syntax = syntax::parse(&codes, &offs, codes.len(), &flags).expect("parses");
        let expected = prog::compile(&syntax, &flags).expect("compiles");
        assert_eq!(decoded.program, expected);
        assert_eq!(decoded.program.ncap, 2);
        assert!(!decoded.program.insts.is_empty());
    }
}
