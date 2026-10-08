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
/// `Path` may be a byte-sized character list or a binary; anything else is
/// `badarg`. `LoadInfo` is forwarded to the hook unchanged. On success the atom
/// `ok` is returned (the module's NIF init function has, by then, run inside the
/// hook); on failure the oracle's `{error, {Reason, Text}}` term is *returned*
/// (see the module docs).
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
