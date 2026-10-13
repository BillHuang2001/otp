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
//! Payload}` form) is returned. `re:version/0`, `re:import/1` (the `export` form
//! back into the `{re_pattern, …}` one, without recompiling) and `re:inspect/2`
//! (`namelist` only) are implemented too, and so are the run-side keys
//! (`run/2,3`, `internal_run/4`), whose bodies live in the [`run`] submodule and
//! drive the [`exec`] matcher and the [`render`] capture renderer.
//!
//! Roughly half of this file is its in-module `#[cfg(test)]` suite, which pins
//! the `compile`/`import`/`inspect` goldens; the implementation above it is a few
//! hundred lines of option/pattern plumbing.

use beam_common::{BeamError, Result};
use beam_term::{atom_val, make_small, signed_value, Eterm, NIL};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

// The compile-side submodules, all live through `compile`/`import`/`inspect`
// and (through `exec`/`render`, below) the run-side keys.
mod options;
mod syntax;
mod prog;
// The match-side submodules, live through the run-side BIF bodies in `run`.
mod exec;
mod render;

// The run-side BIF bodies (`re:run/2,3`, `re:internal_run/4`).
mod run;

#[cfg(test)]
mod oracle;

/// The compiled-pattern tag of the plain `{re_pattern, …}` form.
const RE_PATTERN: &str = "re_pattern";

/// The compiled-pattern tag of the `export` form.
const RE_EXPORTED_PATTERN: &str = "re_exported_pattern";

/// The `namelist` item of `re:inspect/2` — the only item the oracle's `re.erl`
/// implements.
const NAMELIST: &str = "namelist";

/// The arity of both compiled-pattern tuple forms (`{re_pattern, …}` and
/// `{re_exported_pattern, …}`).
const PATTERN_ARITY: usize = 5;

/// The index of the payload image in both compiled-pattern tuple forms.
const PATTERN_IMAGE: usize = 4;

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

/// Whether `term` is the atom `name` in this call's atom table.
fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// The payload image of our compiled-pattern tuple tagged `tag`.
///
/// The two compiled-pattern forms are **not** interchangeable: `re_pattern` is the
/// runnable pattern (`re:run/2,3`, `re:internal_run/4`, `re:inspect/2`) and
/// `re_exported_pattern` the `export` form (`re:import/1`), so the caller names the
/// one tag it accepts and anything else is refused here.
///
/// `None` when `term` is not a 5-tuple, when its tag is not `tag`, or when its
/// element 5 (the payload) is not a byte-aligned binary — the "this is not a pattern
/// of the expected form" test. The image's *contents* are the caller's to validate
/// ([`prog::Payload::decode`] answers a typed `badarg` for a foreign image and
/// never panics).
fn pattern_image(ctx: &BifContext<'_>, term: Eterm, tag: &str) -> Option<Vec<u8>> {
    if !access::is_tuple(term) || access::tuple_size(term) != PATTERN_ARITY {
        return None;
    }
    if !is_atom_named(ctx, access::tuple_element(term, 0), tag) {
        return None;
    }
    bits::bytes(access::tuple_element(term, PATTERN_IMAGE)).ok()
}

/// The unique subpattern names in alphabetical order — the `namelist` item's value.
///
/// `Program::names` is sorted by `(name, group)` and a name may repeat (the
/// `dupnames` option allows two groups to share one), so the list is sorted then
/// deduplicated: `re:inspect(MP, namelist)` reports each name once.
fn unique_names(payload: &prog::Payload) -> Vec<&str> {
    let mut names: Vec<&str> =
        payload.program.names.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// `re:import/1` — rebuild a usable `{re_pattern, …}` from the `export` form.
///
/// The `export` option returns `{re_exported_pattern, Header, Pattern, Options,
/// Payload}` so the pattern survives a trip outside this runtime. Importing it back
/// is cheap (the documented purpose of the form): the payload image already holds
/// the compiled program, so `{re_pattern, NCap, UnicodeFlag, CrlfFlag, Payload}` is
/// rebuilt **around the same image** — no recompilation, and the returned payload
/// decodes to the identical program as the plain `compile/1,2` image.
///
/// # Errors
///
/// [`BeamError::BadArg`] for anything that is not one of our exported patterns: a
/// non-5-tuple, another tag (the oracle raises `badarg` for a plain `{re_pattern,…}`
/// too), a header that is not our image magic, or a payload that is not a
/// byte-aligned binary carrying one of our images.
fn import_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let exported = args[0];
    // The tag must be exactly `re_exported_pattern`; a plain `{re_pattern, …}` is the
    // oracle's `badarg` here as well.
    let image = pattern_image(ctx, exported, RE_EXPORTED_PATTERN).ok_or_else(badarg)?;
    // The header binary is our image magic (`compile` writes it), so a hand-built
    // tuple that merely carries the tag is refused.
    let header = bits::bytes(access::tuple_element(exported, 1)).map_err(|_| badarg())?;
    if header != prog::MAGIC {
        return Err(badarg());
    }
    let payload = prog::Payload::decode(&image).map_err(|_| badarg())?;
    // The flags live in the image, so they are read back rather than trusted from
    // the tuple (`import/1` has no separate flag elements).
    let tag = ctx.intern_atom(RE_PATTERN)?;
    let ncap = ctx.int(i64::from(payload.program.ncap))?;
    let unicode = ctx.int(i64::from(payload.program.flags.unicode))?;
    let crlf = ctx.int(i64::from(payload.program.flags.newline.is_crlf_like()))?;
    let image_term = access::tuple_element(exported, PATTERN_IMAGE);
    ctx.tuple(&[tag, ncap, unicode, crlf, image_term])
}

/// `re:inspect/2` — the `namelist` item, and the typed gap for every other item.
///
/// `namelist` is `{namelist, Names}`: the unique subpattern names as binaries in
/// alphabetical order (group 0 excluded). The oracle's `re.erl` implements **only**
/// this item and raises `badarg` for the others; every other item here (`size`,
/// `options`, `capturecount`, `newline`, …) is the typed `unsupported_bif` rather
/// than a fabricated value, because those describe PCRE2's own compiled blob, which
/// this crate does not have (the payload image is ours — see this node's
/// `CONTEXT.md`).
///
/// The item is examined before the pattern: a non-atom item is `badarg`, and a
/// genuine capability gap is reported even when the pattern is unusable (the
/// oracle's ordering is not observable for a case that is a gap either way).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-atom item, and for a first argument that is not a
/// plain `{re_pattern, …}` tuple (or whose payload image does not decode) — as the
/// oracle does (`re:inspect(zzz, namelist)`, and `re:inspect/2` of the `export` form,
/// which must go through `re:import/1` first); the typed `unsupported_bif` for any
/// other item.
fn inspect_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let item = args[1];
    if !access::is_atom(item) {
        return Err(badarg());
    }
    if !is_atom_named(ctx, item, NAMELIST) {
        return Err(crate::error::unsupported_bif());
    }
    // Only the runnable `{re_pattern, …}` form: the oracle's `inspect/2` refuses the
    // `export` form (`re:inspect(element(2, re:compile(P, [export])), namelist)` is
    // `badarg`), which is `re:import/1`'s input.
    let image = pattern_image(ctx, args[0], RE_PATTERN).ok_or_else(badarg)?;
    let payload = prog::Payload::decode(&image).map_err(|_| badarg())?;
    let mut names: Vec<Eterm> = Vec::new();
    for name in unique_names(&payload) {
        names.push(bits::make_binary(ctx.heap(), name.as_bytes())?);
    }
    let list = ctx.list(&names)?;
    // The `namelist` atom the caller passed is the output tag, so no lookup or
    // interning of the item atom is needed.
    ctx.tuple(&[item, list])
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
    reg.register("re", "internal_run", 4, run::internal_run_4);
    reg.register("re", "run", 2, run::run_2);
    reg.register("re", "run", 3, run::run_3);
    reg.register("re", "version", 0, version_0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::{BeamError, ExitReason};
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
                "namelist",
                "size",
                "dupnames",
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
            self.call_fn("compile", arity, args)
        }

        /// Any `re:<func>` BIF over the fixture's heap and atom table.
        fn call_fn(&mut self, func: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            self.registry.call("re", func, arity, &mut ctx, args)
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

    #[test]
    fn inspect_namelist_lists_the_unique_names_alphabetically() {
        let mut f = Fixture::new();
        // Declaration order is not the reported order: `A` sorts before `B`.
        let out = f.compile_bin(b"(?<B>b)|(?<A>a)", &[]).expect("compiles");
        let mp = f.elem(out, 1);
        let item = f.atom("namelist");
        let got = f.call_fn("inspect", 2, &[mp, item]).expect("inspect");
        assert_eq!(f.arity(got), 2);
        assert!(f.eq(f.elem(got, 0), item), "the item atom tags the result");
        let expected = expected_with_names(&mut f);
        assert!(f.eq(f.elem(got, 1), expected), "alphabetical, as binaries");

        // Two groups may share a name (the `dupnames` option); it is reported once.
        let dupnames = f.atom("dupnames");
        let opts = f.list(&[dupnames]);
        let out = f.compile_with(b"(?<n>a)(?<n>b)", opts).expect("compiles");
        let mp = f.elem(out, 1);
        let item = f.atom("namelist");
        let got = f.call_fn("inspect", 2, &[mp, item]).expect("inspect");
        let n = f.binary(b"n");
        let expected = f.list(&[n]);
        assert!(f.eq(f.elem(got, 1), expected), "a duplicate name collapses");

        // No named group => the empty list.
        let out = f.compile_bin(b"(a)(?:b)", &[]).expect("compiles");
        let mp = f.elem(out, 1);
        let item = f.atom("namelist");
        let got = f.call_fn("inspect", 2, &[mp, item]).expect("inspect");
        assert!(f.eq(f.elem(got, 1), NIL));

        // The `export` form carries the same payload but is not a pattern
        // `inspect/2` accepts; `re:import/1` is what turns it into the plain form
        // the names are readable from.
        let export = f.atom("export");
        let opts = f.list(&[export]);
        let out = f.compile_with(b"(?<B>b)|(?<A>a)", opts).expect("compiles");
        let exported = f.elem(out, 1);
        let item = f.atom("namelist");
        assert!(matches!(
            f.call_fn("inspect", 2, &[exported, item]),
            Err(BeamError::BadArg)
        ));
        let imported = f.call_fn("import", 1, &[exported]).expect("imports");
        let got = f.call_fn("inspect", 2, &[imported, item]).expect("inspect");
        let expected = expected_with_names(&mut f);
        assert!(f.eq(f.elem(got, 1), expected));
    }

    #[test]
    fn inspect_accepts_only_the_plain_pattern_form() {
        let mut f = Fixture::new();
        let namelist = f.atom("namelist");

        // The plain `{re_pattern, …}` form compiles and is what `inspect/2` takes.
        let out = f.compile_bin(b"(?<n>a)", &[]).expect("compiles");
        let mp = f.elem(out, 1);
        let n = f.binary(b"n");
        let expected = f.list(&[n]);
        let got = f.call_fn("inspect", 2, &[mp, namelist]).expect("inspect");
        assert!(f.eq(f.elem(got, 0), namelist));
        assert!(f.eq(f.elem(got, 1), expected));

        // The `export` form must go through `re:import/1` first: the oracle raises
        // `badarg` for it (`re:inspect(element(2, re:compile(P, [export])),
        // namelist)`), so no value is produced here.
        let export = f.atom("export");
        let opts = f.list(&[export]);
        let out = f.compile_with(b"(?<n>a)", opts).expect("compiles");
        let exported = f.elem(out, 1);
        assert!(matches!(
            f.call_fn("inspect", 2, &[exported, namelist]),
            Err(BeamError::BadArg)
        ));

        // A textual pattern is not a compiled pattern either.
        let text = f.binary(b"(?<n>a)");
        assert!(matches!(
            f.call_fn("inspect", 2, &[text, namelist]),
            Err(BeamError::BadArg)
        ));

        // A foreign 5-tuple that merely carries the plain tag (`badarg` because the
        // image does not decode) and one carrying the export tag (refused before the
        // image is even looked at).
        let zero = make_small(0);
        let junk = f.binary(b"not an image");
        let plain_tag = f.atom("re_pattern");
        let fake = f.tuple(&[plain_tag, zero, zero, zero, junk]);
        assert!(matches!(
            f.call_fn("inspect", 2, &[fake, namelist]),
            Err(BeamError::BadArg)
        ));
        let exported_tag = f.atom("re_exported_pattern");
        let fake = f.tuple(&[exported_tag, zero, zero, zero, junk]);
        assert!(matches!(
            f.call_fn("inspect", 2, &[fake, namelist]),
            Err(BeamError::BadArg)
        ));
    }

    /// `[<<"A">>, <<"B">>]` — the namelist of `(?<B>b)|(?<A>a)`.
    fn expected_with_names(f: &mut Fixture) -> Eterm {
        let a = f.binary(b"A");
        let b = f.binary(b"B");
        f.list(&[a, b])
    }

    #[test]
    fn inspect_reports_a_gap_for_every_other_item_and_raises_badarg_for_a_bad_input() {
        let mut f = Fixture::new();
        let out = f.compile_bin(b"(?<A>A)", &[]).expect("compiles");
        let mp = f.elem(out, 1);

        // The oracle implements only `namelist`; any other item is our typed gap
        // (never a fabricated PCRE2-internals value).
        let size = f.atom("size");
        match f.call_fn("inspect", 2, &[mp, size]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected the typed unsupported_bif gap, got {other:?}"),
        }

        // A non-atom item, and a first argument that is not one of our patterns.
        let namelist = f.atom("namelist");
        let one = make_small(1);
        assert!(matches!(f.call_fn("inspect", 2, &[mp, one]), Err(BeamError::BadArg)));
        assert!(matches!(
            f.call_fn("inspect", 2, &[one, namelist]),
            Err(BeamError::BadArg)
        ));
        let nope = f.atom("nope");
        assert!(matches!(
            f.call_fn("inspect", 2, &[nope, namelist]),
            Err(BeamError::BadArg)
        ));

        // A `{re_pattern, …}` tuple whose element 5 is not one of our images.
        let tag = f.atom("re_pattern");
        let junk = f.binary(b"not an image");
        let zero = make_small(0);
        let fake = f.tuple(&[tag, zero, zero, zero, junk]);
        assert!(matches!(
            f.call_fn("inspect", 2, &[fake, namelist]),
            Err(BeamError::BadArg)
        ));
        // ... and a 5-tuple tagged with our export tag but carrying a foreign image.
        let exported_tag = f.atom("re_exported_pattern");
        let wrong_image = f.tuple(&[exported_tag, zero, zero, zero, junk]);
        assert!(matches!(
            f.call_fn("inspect", 2, &[wrong_image, namelist]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn import_rebuilds_the_pattern_around_the_exported_payload() {
        let mut f = Fixture::new();
        let export = f.atom("export");
        let opts = f.list(&[export]);
        let out = f.compile_with(b"(?<n>a)(b)", opts).expect("compiles");
        let exported = f.elem(out, 1);

        let imported = f.call_fn("import", 1, &[exported]).expect("imports");
        assert_eq!(f.arity(imported), 5);
        assert!(f.eq(f.elem(imported, 0), f.atom("re_pattern")));
        assert_eq!(f.small(f.elem(imported, 1)), 2, "NCap");
        assert_eq!(f.small(f.elem(imported, 2)), 0, "no unicode");
        assert_eq!(f.small(f.elem(imported, 3)), 0, "newline lf");
        // The image is the exported one, unchanged (importing does not recompile),
        // and it decodes to the program a plain `compile/1` of the same pattern
        // produces.
        let exported_image = f.elem(exported, 4);
        assert!(f.eq(f.elem(imported, 4), exported_image));
        let plain = f.compile_bin(b"(?<n>a)(b)", &[]).expect("compiles");
        let plain_mp = f.elem(plain, 1);
        let decoded = prog::Payload::decode(&f.image(imported)).expect("decodes");
        let expected = prog::Payload::decode(&f.image(plain_mp)).expect("decodes");
        assert_eq!(decoded.program, expected.program);
        assert_eq!(decoded.source.as_slice(), b"(?<n>a)(b)");
        assert!(f.eq(imported, plain_mp), "the rebuilt tuple is the compile/1 one");

        // A unicode + `{newline, crlf}` pattern carries both flags through the image.
        let unicode = f.atom("unicode");
        let newline = f.atom("newline");
        let crlf = f.atom("crlf");
        let newline_opt = f.tuple(&[newline, crlf]);
        let opts = f.list(&[export, unicode, newline_opt]);
        let out = f.compile_with(b"(a)", opts).expect("compiles");
        let exported = f.elem(out, 1);
        let imported = f.call_fn("import", 1, &[exported]).expect("imports");
        assert_eq!(f.small(f.elem(imported, 2)), 1, "the unicode flag");
        assert_eq!(f.small(f.elem(imported, 3)), 1, "the crlf flag");

        // `import/1` of anything that is not one of our exported patterns is badarg:
        // a plain `{re_pattern, …}`, a bare integer, and a tuple that carries the
        // tag but not our header/header image.
        let plain = f.compile_bin(b"a", &[]).expect("compiles");
        let plain_mp = f.elem(plain, 1);
        assert!(matches!(f.call_fn("import", 1, &[plain_mp]), Err(BeamError::BadArg)));
        let seven = make_small(7);
        assert!(matches!(f.call_fn("import", 1, &[seven]), Err(BeamError::BadArg)));
        let image = f.elem(exported, 4);
        let junk = f.binary(b"not an image");
        let tag = f.atom("re_exported_pattern");
        let fake = f.tuple(&[tag, junk, image, image, image]);
        assert!(
            matches!(f.call_fn("import", 1, &[fake]), Err(BeamError::BadArg)),
            "the header must be our image magic"
        );
        let header = f.binary(prog::MAGIC);
        let fake = f.tuple(&[tag, header, junk, junk, junk]);
        assert!(
            matches!(f.call_fn("import", 1, &[fake]), Err(BeamError::BadArg)),
            "the payload must be one of our images"
        );
    }
}
