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

//! The two public entry points: [`load`] and [`load_file`].
//!
//! `load` is the whole pipeline — container → chunks → instruction decode →
//! lowering — and `load_file` is the convenience `std::fs::read` wrapper. These are
//! the *only* entry points; every intermediate type is public for inspection and
//! testing (see the crate `CONTEXT.md`, "API Surface").
use std::path::Path;

use beam_common::Result;

use crate::error::bad_file;
use crate::ir::{Atom, Module};

/// Loads a `.beam` image from memory into the IR [`Module`].
///
/// Parses the `FOR1` container, decodes every known chunk, decodes the `Code`
/// instruction stream, lowers the result into the typed block-structured IR and
/// materializes the `LitT` static literals into terms ([`crate::literal`]); the
/// `FunT` lambda descriptors are lowered onto [`Module::funs`].
///
/// A malformed or truncated image yields `BeamError::BadArg`; an over-large
/// size/arity/count bound yields `BeamError::SystemLimit`. Never panics.
pub fn load(bytes: &[u8]) -> Result<Module> {
    let container = crate::container::parse(bytes)?;
    let chunks = crate::chunk::parse(&container)?;

    // `atoms[0]` is the NIL placeholder and `atoms[1]` the module name; an image
    // without a module name is not loadable.
    if chunks.atoms.len() < 2 {
        return Err(bad_file("atom table has no module name"));
    }

    // The atom map the decoder resolves `a` operands against. It is interned here
    // (not in `build_module`) because the decoder needs it first; `build_module`
    // re-interns the same names, which is idempotent and yields the same atoms.
    let mut atom_table = beam_term::AtomTable::new();
    let mut atoms: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
    atoms.push(Atom::MAX);
    for name in &chunks.atoms[1..] {
        atoms.push(atom_table.intern(name));
    }

    let raw = crate::decode::decode_stream(
        chunks.code_stream,
        &atoms,
        &chunks.code_version,
        &chunks.types,
    )?;

    crate::lower::build_module(&chunks, &raw, atom_table)
}

/// Loads a `.beam` image from a file path into the IR [`Module`].
///
/// Reads the whole file and delegates to [`load`]; an operating-system read error
/// is surfaced as [`beam_common::BeamError::Io`].
pub fn load_file(path: impl AsRef<Path>) -> Result<Module> {
    let bytes = std::fs::read(path)?;
    load(&bytes)
}
