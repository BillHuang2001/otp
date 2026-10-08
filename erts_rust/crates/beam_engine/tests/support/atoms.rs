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

//! **The one shared atom table** the host interns every loaded module into.
//!
//! Split out of the former single-file `support/mod.rs`; [`SEED_ATOMS`] is
//! re-exported unchanged by the [`support`](super) facade.
//!
//! A real runtime interns atoms globally; [`beam_loader::load_file`] would build
//! a *private* [`AtomTable`] per module (indices assigned in wire order), so the
//! same name (`true`, `ok`, …) would get different indices in different modules
//! — and `beam_term::cmp` resolves atom *names* through the running module's
//! table, so a cross-module atom comparison would silently fail. This module
//! therefore re-runs the loader's own public pipeline seeded with **one shared
//! [`AtomTable`]**, and seeds the runtime's globally interned well-known atoms
//! into every module.

use beam_common::Result;
use beam_loader::{Atom, Module};
use beam_term::AtomTable;

/// The runtime's **well-known atoms** — the C `error_atom[NUMBER_EXIT_CODES]`
/// table (`../../erts/emulator/beam/beam_common.c:382-406`) plus the
/// exception-*class* and auxiliary atoms the exceptions family resolves by name
/// (`src/families/exceptions/`: `install_class_atom`, `catch_end`'s `'EXIT'`,
/// `build_stacktrace`'s `file`/`line`).
///
/// A real runtime keeps these **globally interned**, so they are nameable from
/// *every* module whether or not the module's own source ever mentions them. The
/// real OTP compiler does **not** emit them into a module's atom chunk — e.g.
/// `erlc p.erl` on a module whose functions raise `function_clause` /
/// `{case_clause,_}` / `if_clause` / `{badmatch,_}` / `{try_clause,_}` still
/// produces an atom list containing none of them — so a module loaded on its own
/// cannot name its own exception reasons. Seeding them into every loaded module's
/// atom table is this host's analogue of the runtime's global interning; the
/// production counterpart is `beam_rt::boot::CORE_ATOMS`.
///
/// The engine's lookup policy is deliberately **unchanged**: it still resolves a
/// reason atom by name from the running module's table and reports a typed
/// `unsupported` when the name is absent, so a *different* atom is never silently
/// substituted.
///
/// Exposed (`pub`) so a test can assert the exact seed set that the host interns.
pub const SEED_ATOMS: &[&str] = &[
    // The C `error_atom` table, in file order (`beam_common.c:382-406`).
    "internal_error",
    "normal",
    "badarg",
    "badarith",
    "badmatch",
    "function_clause",
    "case_clause",
    "if_clause",
    "undef",
    "badfun",
    "badarity",
    "timeout_value",
    "noproc",
    "notalive",
    "system_limit",
    "try_clause",
    "notsup",
    "badmap",
    "badkey",
    "badrecord",
    "badfield",
    "novalue",
    // The exception *class* atoms a landing installs into `x(3)` (`error`/`exit`/
    // `throw`), plus the auxiliaries the family looks up by name: `'EXIT'`
    // (`catch_end`'s `{'EXIT',Reason}`), `file`/`line` (`build_stacktrace`'s
    // source location).
    "error",
    "exit",
    "throw",
    "EXIT",
    "file",
    "line",
];

/// Loads a `.beam` image into the IR, interning its atoms (and its decoded
/// literal atoms) into `atoms` — the host's one shared [`AtomTable`].
///
/// This is [`beam_loader::load`]'s exact pipeline, re-run through the loader's
/// public modules so the shared table (not a private per-module one) is the
/// interning context. See the module docs for why that matters.
///
/// # The index-consistency invariant
///
/// After this returns, **for every index `i` in the module's atom table,
/// `module.atom_table.name(i) == atoms.name(i)`** — the shared table and every
/// loaded module agree on `Atom -> name` for all of the module's indices. That is
/// what makes a term built by one module compare equal to the same-named atom
/// interned by another (and lets the tests resolve names through
/// `ModuleHost::atom_table`). The invariant is maintained by APPENDING to the
/// shared table only:
/// * the wire loop pre-interns the chunk atoms into `atoms`, so `build_module`'s
///   clone already holds them at the same indices;
/// * `build_module` can only *append* — a literal-only atom is new to the clone,
///   so it lands at `atoms.len()..`, and re-interning it here in index order
///   appends it to `atoms` at exactly the same index;
/// * the [`SEED_ATOMS`] are then appended to both tables in lockstep (each name is
///   interned into `atoms` first and into the module's own table second; both
///   tables are equal-length and consistent before each iteration, so both return
///   the same index whether the name is new or already present).
///
/// Existing indices are therefore never shifted, so atom-keyed structures built by
/// earlier modules (e.g. flat-map storage order) are untouched.
pub(super) fn load_with_shared_atoms(bytes: &[u8], atoms: &mut AtomTable) -> Result<Module> {
    use beam_loader::{chunk, container, decode, lower};

    let container = container::parse(bytes)?;
    let chunks = chunk::parse(&container)?;

    // `atoms[0]` is the NIL placeholder; `atoms[1]` is the module name (mirrors
    // `load`). Interning is idempotent, so a name shared with an earlier module
    // keeps that module's index.
    let mut wire: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
    wire.push(Atom::MAX);
    for name in &chunks.atoms[1..] {
        wire.push(atoms.intern(name));
    }

    let raw = decode::decode_stream(
        chunks.code_stream,
        &wire,
        &chunks.code_version,
        &chunks.types,
    )?;

    // `build_module` owns (and may extend) a clone of the shared table while it
    // decodes the `LitT` literals; any atom that appeared only in a literal is
    // appended after the clone's prefix at exactly `atoms.len()..`, so merging
    // them back in index order keeps the shared indices identical.
    let mut module = lower::build_module(&chunks, &raw, atoms.clone())?;
    let base = atoms.len();
    for index in base..module.atom_table.len() {
        if let Some(name) = module.atom_table.name(Atom::from_raw(index as u32)) {
            atoms.intern(name);
        }
    }

    // Append the runtime's well-known atoms to the module's own table, in lockstep
    // with the shared table so the index-consistency invariant holds (see above).
    // `atoms` is interned first; because the two tables are equal-length and
    // consistent for all their indices, the module's table resolves the same name
    // to the same index — whether it is already present or newly appended.
    for &name in SEED_ATOMS {
        let shared = atoms.intern(name);
        let local = module.atom_table.intern(name);
        debug_assert_eq!(
            shared, local,
            "seed atom `{name}` broke the module/shared atom-table index consistency"
        );
    }

    Ok(module)
}
