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

//! `erlang:load_nif/2` — loading a NIF library for the calling module.
//!
//! Ground truth (read-only): the `erlang:load_nif/2` entry in
//! `../../erts/emulator/beam/bif.tab` (line 527) and
//! `../../erts/emulator/beam/erl_nif.c` (`erts_load_nif`/`load_nif_error`,
//! lines 4455-4830).
//!
//! # A NIF library is runtime machinery, not a BIF
//!
//! `load_nif` `dlopen`s a shared object and binds its NIF/driver entry points into
//! the emulator's ABI — Stage-5 machinery (`beam_rt`/`beam_abi`), never a BIF. So
//! this BIF routes the load through the optional [`NifLoader`](crate::context::NifLoader)
//! seam ([`BifContext::nif_loader`](crate::context::BifContext::nif_loader)), which
//! `beam_rt` supplies. `Path` is decoded to a Rust `String` and `LoadInfo` is
//! forwarded verbatim; the hook owns the shared-object suffix it appends.
//!
//! # A load failure is a *returned* `{error, {Reason, Text}}`
//!
//! `erlang:load_nif/2` **returns** `{error, {Reason, Text}}` on failure — it does not
//! raise (verified against the system-ERTS oracle: a direct call from a compiled
//! module yields `{returned,{error,{load_failed, "Failed to load NIF library: …"}}}`).
//! `Reason` is one of `load_failed`, `bad_lib`, `reload`, `load`, `upgrade`,
//! `old_code` (named by the [`NifLoadError`](crate::context::NifLoadError) enum) and
//! `Text` an Erlang string; the calling module's NIF-init wrapper is what turns it
//! into a raise, if it chooses to.
//!
//! With no hook installed — or an unknown calling module — the BIF reports a typed
//! `unsupported_bif` raise (workspace `CONTEXT.md` §3): never a fabricated `ok`,
//! never a silently dropped load.
//!
//! # The `Path` type contract: the two accepted shapes, and the `badarg` *raise*
//!
//! The oracle converts `Path` with `erts_convert_filename_to_encoding(filename, …,
//! allow_empty = 1, allow_atom = 0, …)` (`erl_nif.c:4757`), so **exactly two** shapes are
//! accepted — a character list (an empty list included) and a bitstring — while everything
//! else, **an atom included**, makes that conversion return `NULL`, which `erts_load_nif`
//! turns into `THE_NON_VALUE` and the `load_nif` instruction into `BIF_ERROR(…, am_badarg)`.
//! So a non-string `Path` is a *catchable* `error:badarg` — a **raise**, not a `badarg`
//! *value* and never a `{error, {load_failed, _}}` term. Measured on the system oracle
//! (OTP 29.1.1 / erts-17.1, from a compiled module's own `erlang:load_nif/2` call):
//!
//! | `Path` | oracle answer |
//! |---|---|
//! | `42`, `{a}`, `atom_path` | `error:badarg` (raised) |
//! | `"/nonexistent/foo"` | `{error,{load_failed,"Failed to load NIF library: '/nonexistent/foo.so: cannot open shared object file: No such file or directory'"}}` |
//! | `<<"/nonexistent/foo">>` (binary) | same as the string form |
//! | `"/nonexistent/foo.so"` | `…'/nonexistent/foo.so.so: cannot open…'` (the suffix is appended unconditionally) |
//! | `NoInit.so` with no `nif_init` | `{error,{bad_lib,"Failed to find library init function: '<path>.so: undefined symbol: _nif_init'"}}` |
//!
//! The suffix and every word of those texts belong to the loader
//! ([`NifLoader`](crate::context::NifLoader), `beam_abi`'s `nif::loader`); this BIF decodes
//! the two accepted shapes, forwards the resulting string, and renders whatever
//! [`NifLoadError`](crate::context::NifLoadError) comes back as the returned term.

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access;
use crate::bits;
use crate::context::{BifContext, NifLoadError};
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// The `erlang:load_nif/2` BIF.
///
/// `Path` may be a byte-sized character list or a binary; anything else — an **atom**
/// included, exactly as the oracle's `allow_atom = 0` conversion has it — is `badarg`,
/// raised (`BIF_ERROR(…, am_badarg)`), never returned. `LoadInfo` is forwarded to the hook
/// unchanged. On success the atom `ok` is returned (the module's NIF init function has, by
/// then, run inside the hook); on failure the oracle's `{error, {Reason, Text}}` term is
/// *returned* (see the module docs).
fn load_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // A non-string Path is `badarg` before the seam is consulted.
    let path = path_string(args[0])?;
    let load_info = args[1];

    // Copy the module name to an owned `String` *before* taking the mutable
    // `nif_loader()` borrow (it borrows the context for `'a`).
    let module = match ctx.current_module() {
        Some(module) => module.to_string(),
        None => return Err(unsupported_bif()),
    };

    let outcome = match ctx.nif_loader() {
        Some(loader) => loader.load_nif(&module, &path, load_info),
        None => return Err(unsupported_bif()),
    };

    match outcome {
        Ok(()) => ctx.ok(),
        Err(error) => load_error(ctx, error),
    }
}

/// The `Path` argument as a Rust `String`: a byte-sized character list or a binary
/// (the two shapes the oracle accepts).
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for a non-list/non-binary term, a non-small-int list
/// element, an element outside `0..=255`, or bytes that are not UTF-8.
fn path_string(term: Eterm) -> Result<String> {
    if bits::is_binary(term) {
        let bytes = bits::bytes(term)?;
        return String::from_utf8(bytes).map_err(|_| badarg());
    }
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        bytes.push(u8::try_from(signed_value(elem)).map_err(|_| badarg())?);
    }
    String::from_utf8(bytes).map_err(|_| badarg())
}

/// The `{error, {Reason, Text}}` term for a load failure — the oracle's
/// `load_nif_error` shape, *returned* rather than raised.
///
/// `Text` is an Erlang string (a proper list of the message's code points) and
/// `Reason` the atom naming the [`NifLoadError`] variant.
///
/// The `error` tag atom and the `Reason` atom are **interned on demand**
/// ([`BifContext::intern_atom`]) — the oracle's `load_nif_error` builds them with
/// `mkatom`/`atom_put` (`erl_nif.c:4479-4482`), so neither need be pre-seeded. An
/// already-interned name resolves without the hook; a new one needs an
/// `AtomInterner` (which `beam_rt` installs).
///
/// # Errors
///
/// Propagates a heap-allocation failure, and [`beam_common::BeamError::TermNotFound`] when a
/// reason/`error` atom is not interned and no `AtomInterner` hook is installed —
/// never a substituted atom.
fn load_error(ctx: &mut BifContext<'_>, error: NifLoadError) -> Result<Eterm> {
    let (reason, text) = match error {
        NifLoadError::LoadFailed(text) => ("load_failed", text),
        NifLoadError::BadLib(text) => ("bad_lib", text),
        NifLoadError::Reload(text) => ("reload", text),
        NifLoadError::Load(text) => ("load", text),
        NifLoadError::Upgrade(text) => ("upgrade", text),
        NifLoadError::OldCode(text) => ("old_code", text),
    };
    let tag = ctx.intern_atom("error")?;
    let reason = ctx.intern_atom(reason)?;
    let text = text_string(ctx, &text)?;
    let inner = ctx.tuple(&[reason, text])?;
    ctx.tuple(&[tag, inner])
}

/// The `Text` element: a proper list of the message's code points.
///
/// # Errors
///
/// Propagates a heap-allocation failure (or [`beam_common::BeamError::SystemLimit`] for a code
/// point beyond the 60-bit small range, which cannot occur for a Unicode scalar).
fn text_string(ctx: &mut BifContext<'_>, text: &str) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(text.chars().count());
    for ch in text.chars() {
        elems.push(ctx.int(i64::from(ch as u32))?);
    }
    ctx.list(&elems)
}

/// Registers the `erlang:load_nif/2` BIF.
pub(crate) fn register(registry: &mut BifRegistry) {
    registry.register("erlang", "load_nif", 2, load_nif);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::{BeamError, ExceptionClass};
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "error", "nif.so"]);
        atoms
    }

    #[test]
    fn a_byte_list_path_decodes_to_a_string() {
        let atoms = seeded_atoms();
        let mut heap = Heap::new(64).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let elems: Vec<Eterm> = b"nif.so"
            .iter()
            .map(|b| ctx.int(i64::from(*b)).expect("a byte"))
            .collect();
        let list = ctx.list(&elems).expect("fits");
        assert_eq!(path_string(list).expect("a string"), "nif.so");
    }

    #[test]
    fn a_non_string_path_is_badarg() {
        let atoms = seeded_atoms();
        let mut heap = Heap::new(64).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert!(matches!(path_string(make_small(1)), Err(BeamError::BadArg)));
        let tuple = ctx.tuple(&[make_small(1)]).expect("fits");
        assert!(matches!(path_string(tuple), Err(BeamError::BadArg)));
        // A list element outside `0..=255` is `badarg` too.
        let bad = ctx.list(&[make_small(256)]).expect("fits");
        assert!(matches!(path_string(bad), Err(BeamError::BadArg)));
    }

    #[test]
    fn register_installs_load_nif_2() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "load_nif", 2));
        assert_eq!(reg.len(), 1);
    }

    /// A `NifLoader` double: it records the path it was handed and replays one scripted result.
    struct ScriptedLoader {
        result: std::result::Result<(), NifLoadError>,
        paths: Vec<String>,
    }

    impl ScriptedLoader {
        fn new(result: std::result::Result<(), NifLoadError>) -> ScriptedLoader {
            ScriptedLoader {
                result,
                paths: Vec::new(),
            }
        }
    }

    impl crate::context::NifLoader for ScriptedLoader {
        fn load_nif(
            &mut self,
            _module: &str,
            path: &str,
            _load_info: Eterm,
        ) -> std::result::Result<(), NifLoadError> {
            self.paths.push(path.to_string());
            self.result.clone()
        }
    }

    /// The `{error, {Reason, Text}}` term's `(reason, text)` read back as Rust strings.
    fn read_error_term(atoms: &AtomTable, term: Eterm) -> (String, String) {
        use beam_codec::term::{cons_head, cons_tail, tuple_arity, tuple_element};
        assert_eq!(tuple_arity(term).expect("a tuple"), 2, "{term:?}");
        let tag = tuple_element(term, 0).expect("the tag");
        assert_eq!(
            atoms.name(beam_term::atom_val(tag)),
            Some("error"),
            "the tag is `error`"
        );
        let inner = tuple_element(term, 1).expect("the inner tuple");
        assert_eq!(tuple_arity(inner).expect("a tuple"), 2);
        let reason = tuple_element(inner, 0).expect("the reason");
        let text = tuple_element(inner, 1).expect("the text");
        let mut rendered = String::new();
        let mut cur = text;
        while beam_term::is_list(cur) {
            let head = cons_head(cur).expect("a cons head");
            rendered.push(
                u8::try_from(signed_value(head))
                    .expect("an ASCII code point")
                    .into(),
            );
            cur = cons_tail(cur).expect("a cons tail");
        }
        assert!(beam_term::is_nil(cur), "the text is a proper list");
        let reason = atoms
            .name(beam_term::atom_val(reason))
            .expect("a named reason atom")
            .to_string();
        (reason, rendered)
    }

    /// `Path` shapes that are neither a character list nor a bitstring are the oracle's
    /// **catchable `error:badarg`** — raised, not returned, and never a `{error, _}` value.
    ///
    /// An **atom is one of them**: `erts_load_nif` converts the path with `allow_atom = 0`
    /// (`erl_nif.c:4757`), so `erlang:load_nif(atom_path, [])` raises `badarg` on the system
    /// oracle (measured, OTP 29.1.1) rather than reporting a load failure for `atom_path.so`.
    #[test]
    fn a_path_that_is_not_a_string_raises_badarg_and_never_reaches_the_loader() {
        let mut atoms = seeded_atoms();
        atoms.seed(["atom_path"]);
        let mut heap = Heap::new(64).expect("test heap");
        let mut loader = ScriptedLoader::new(Ok(()));
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_current_module("m")
            .with_nif_loader(&mut loader);

        for path in [make_small(42), make_small(1)] {
            assert!(matches!(
                load_nif(&mut ctx, &[path, make_small(0)]),
                Err(BeamError::BadArg)
            ));
        }
        let tuple = ctx.tuple(&[make_small(1)]).expect("fits");
        assert!(matches!(
            load_nif(&mut ctx, &[tuple, make_small(0)]),
            Err(BeamError::BadArg)
        ));
        let atom = ctx.atom("atom_path").expect("seeded");
        assert!(matches!(
            load_nif(&mut ctx, &[atom, make_small(0)]),
            Err(BeamError::BadArg)
        ));
        assert!(
            loader.paths.is_empty(),
            "a badarg Path never reaches the loader: {:?}",
            loader.paths
        );
    }

    /// A **binary** path is accepted exactly like a character list and reaches the loader
    /// with the same decoded string (the oracle's `is_bitstring` arm,
    /// `erts_convert_filename_to_encoding`).
    #[test]
    fn a_binary_path_reaches_the_loader_like_a_string() {
        let mut atoms = seeded_atoms();
        atoms.seed(["load_failed"]);
        let mut heap = Heap::new(256).expect("test heap");
        let mut loader = ScriptedLoader::new(Err(NifLoadError::LoadFailed(
            "Failed to load NIF library: '/nonexistent/foo.so: cannot open shared object file: No such file or directory'".to_string(),
        )));
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_current_module("m")
            .with_nif_loader(&mut loader);

        let binary = beam_codec::term::make_binary(ctx.heap(), b"/nonexistent/foo").expect("fits");
        let term = load_nif(&mut ctx, &[binary, make_small(0)])
            .expect("a load failure is returned, not raised");
        assert_eq!(loader.paths, vec!["/nonexistent/foo".to_string()]);

        // The failure text is the loader's, verbatim: the suffix (`.so`) and every word of the
        // message are the loader's business, this BIF renders them as a string it returns.
        let (reason, text) = read_error_term(&atoms, term);
        assert_eq!(reason, "load_failed");
        assert_eq!(
            text,
            "Failed to load NIF library: '/nonexistent/foo.so: cannot open shared object file: No such file or directory'"
        );
    }

    /// A `bad_lib` from a `.so` that exports no init function is the C's exact text, returned
    /// as a value — measured on the oracle as
    /// `{error,{bad_lib,"Failed to find library init function: '<path>.so: undefined symbol: _nif_init'"}}`.
    #[test]
    fn a_missing_nif_init_renders_the_oracles_bad_lib_text_as_a_value() {
        let mut atoms = seeded_atoms();
        atoms.seed(["bad_lib"]);
        let mut heap = Heap::new(256).expect("test heap");
        let mut loader = ScriptedLoader::new(Err(NifLoadError::BadLib(
            "Failed to find library init function: '/tmp/noinit.so: undefined symbol: _nif_init'"
                .to_string(),
        )));
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_current_module("m")
            .with_nif_loader(&mut loader);

        let elems: Vec<Eterm> = b"/tmp/noinit"
            .iter()
            .map(|b| ctx.int(i64::from(*b)).expect("a byte"))
            .collect();
        let path = ctx.list(&elems).expect("fits");
        let term =
            load_nif(&mut ctx, &[path, make_small(0)]).expect("a load failure is returned");
        assert_eq!(loader.paths, vec!["/tmp/noinit".to_string()]);
        let (reason, text) = read_error_term(&atoms, term);
        assert_eq!(reason, "bad_lib");
        assert_eq!(
            text,
            "Failed to find library init function: '/tmp/noinit.so: undefined symbol: _nif_init'"
        );
    }

    /// An **empty** path is legal (`allow_empty = 1`): the empty string reaches the loader,
    /// which appends `.so` and reports the oracle's `'.so: cannot open …'`.
    #[test]
    fn an_empty_path_is_accepted_and_reaches_the_loader() {
        let atoms = seeded_atoms();
        let mut heap = Heap::new(64).expect("test heap");
        let mut loader = ScriptedLoader::new(Ok(()));
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_current_module("m")
            .with_nif_loader(&mut loader);

        let ok = ctx.ok().expect("fits");
        let term = load_nif(&mut ctx, &[NIL, make_small(0)]).expect("an empty path loads");
        assert_eq!(term, ok, "a successful load answers `ok`");
        let empty_binary = beam_codec::term::make_binary(ctx.heap(), b"").expect("fits");
        let term = load_nif(&mut ctx, &[empty_binary, make_small(0)]).expect("an empty binary loads");
        assert_eq!(term, ok);
        assert_eq!(loader.paths, vec![String::new(), String::new()]);
    }
    #[test]
    fn without_a_seam_the_bif_is_a_typed_unsupported() {
        let atoms = seeded_atoms();
        let mut heap = Heap::new(64).expect("test heap");
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_current_module("m");

        match load_nif(&mut ctx, &[NIL, make_small(0)]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, ExceptionClass::Error);
                assert_eq!(
                    raise.reason,
                    beam_common::ExitReason::atom("unsupported_bif")
                );
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }
}
