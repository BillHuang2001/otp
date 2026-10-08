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

//! The runtime's concrete [`ModuleInfo`] hook: the loaded-module data behind
//! `erlang:get_module_info/1,2`.
//!
//! `beam_bifs`'s `modinfo.rs` decodes the two arguments and validates the item against
//! the C's dispatch chain, but the *answers* live with the **loaded**
//! [`beam_loader::Module`] owned by the runtime's [`CodeServer`] — Stage-5 state no BIF
//! can reach. It therefore routes every item through the [`ModuleInfo`] seam, and this
//! module is the concrete provider
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) installs
//! over the code server (the same `&CodeServer` borrow pattern as
//! [`RecordBridge`](super::bridges::RecordBridge)).
//!
//! An **unknown module** answers `Ok(None)` (the oracle's `badarg`); the item name is
//! already validated by the BIF layer, and any item this bridge cannot render is an
//! honest typed capability gap — never a fabricated value (workspace `CONTEXT.md` §3).
//!
//! # Atoms resolve by *name* into the caller's shared table
//!
//! The seam hands this bridge the caller's shared [`AtomTable`] **immutably**, so it may
//! never intern. Every atom it emits is therefore resolved by name through
//! `AtomTable::lookup`; a name the table does not hold is a typed capability gap, never a
//! wrong index. A module the code server has loaded has all of its own `AtU8`/literal
//! atoms interned into that shared table (see [`CodeServer::load_bytes`]), so a module's
//! export/function names always resolve.
//!
//! The `Attr`/`CInf` chunks are **raw `term_to_binary` images** (`VERSION_MAGIC 131`,
//! `beam_loader::chunk::misc::raw`) whose atoms include compiler keys (`vsn`, `version`,
//! `options`, `source`, …) that are not part of a module's own `AtU8`. Since the decoder
//! can only intern into a table it is *given*, the chunk is decoded into a **scratch**
//! atom table and heap and then [`remap`]ped onto the caller's heap, resolving every atom
//! by name — so a chunk whose atoms are all seeded (see `crate::boot::CORE_ATOMS`) is
//! reproduced exactly, and one whose atoms are not is an honest gap.
//!
//! # The `exports` ordering divergence (documented)
//!
//! The oracle's `exports` is the **global ERTS export-table** order
//! (`../../erts/emulator/beam/erl_bif_info.c:8434-8471` walks `export_list`) — not the
//! file `ExpT` order, not sorted, and not reconstructible from a single module. This
//! bridge therefore returns the module's **real export set in the loader's
//! `Module::exports` order** (real data, possibly a different order than the oracle for
//! some modules). It never fabricates or drops an entry; the differential tests compare
//! `exports` as an unordered **set** for exactly this reason.

use beam_alloc::Heap;
use beam_bifs::ModuleInfo;
use beam_common::{BeamError, Result};
use beam_codec::{term, Decoder};
use beam_loader::{Export, Function, Module};
use beam_term::prelude::{is_atom, is_immediate, is_nil, is_small, make_atom, NIL};
use beam_term::{atom_val, try_make_small, AtomTable, Eterm};

use crate::codeserver::CodeServer;
use crate::error;

use super::module_md5;

/// The scratch heap the `Attr`/`CInf` chunk is first decoded onto.
///
/// The chunks are small (an attribute list and the compiler's version/option list), so a
/// few thousand words is ample; a chunk that overflows it is a typed allocation error
/// rather than a truncated term.
const CHUNK_SCRATCH_WORDS: usize = 8_192;

/// The runtime's concrete [`ModuleInfo`] hook: a read-only borrow of the code server.
///
/// It borrows only the [`CodeServer`], so it cannot co-exist with
/// [`FunBridge`](super::bridges::FunBridge)'s `&mut Runtime` borrow or
/// [`DictBridge`](super::bridges::DictBridge)'s `&mut ProcessTable` borrow —
/// [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) installs
/// exactly one bridge. `get_module_info/1,2` applies no fun argument and touches no
/// dictionary or global table, so nothing is lost.
pub(super) struct ModuleInfoBridge<'a> {
    /// The code server holding the loaded modules.
    pub(super) code_server: &'a CodeServer,
}

impl ModuleInfo for ModuleInfoBridge<'_> {
    fn module_info(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        module: &str,
        item: &str,
    ) -> Result<Option<Eterm>> {
        // A module absent from the code server is not loaded ⇒ the oracle's `badarg`
        // (`ModuleInfo` answers `None`).
        let Some(loaded) = self.code_server.module(module) else {
            return Ok(None);
        };
        let value = match item {
            "module" => atom_term(atoms, module)?,
            "exports" => export_list(heap, atoms, loaded)?,
            "functions" => function_list(heap, atoms, loaded)?,
            // No NIF library is loaded for a module in this pass, and no native code is
            // emitted ⇒ both oracle lists are empty (`native` is the `false` atom).
            "nifs" | "native_addresses" => NIL,
            "native" => atom_term(atoms, "false")?,
            "md5" => md5(heap, self.code_server, module)?,
            "attributes" => decode_chunk(heap, atoms, loaded.attributes.as_deref())?,
            "compile" => decode_chunk(heap, atoms, loaded.compile_info.as_deref())?,
            // The BIF layer validates the item against the C's set before it consults the
            // seam, so this is unreachable; answer `None` (the oracle's `badarg`) rather
            // than a fabricated item.
            _ => return Ok(None),
        };
        Ok(Some(value))
    }
}

/// An interned atom's term, resolved by **name** through `atoms` (never interned here).
///
/// # Errors
///
/// A typed capability gap when the name is not interned in `atoms` — never a substituted
/// or invented atom index.
fn atom_term(atoms: &AtomTable, name: &str) -> Result<Eterm> {
    match atoms.lookup(name) {
        Some(atom) => Ok(make_atom(atom)),
        None => Err(error::unsupported(&format!(
            "get_module_info: the atom `{name}` is not interned"
        ))),
    }
}

/// The name of `atom` as the **module's own** atom table spells it.
///
/// # Errors
///
/// A typed capability gap for an atom the module's table cannot name (a corrupt/mismatched
/// module), rather than a wrong name.
fn module_atom_name(module: &Module, atom: beam_term::Atom) -> Result<&str> {
    module.atom_name(atom).ok_or_else(|| {
        error::unsupported("get_module_info: a module atom has no name in its own table")
    })
}

/// The `exports` item: `[{Function, Arity}]` over the module's real export set, in the
/// loader's order (see the module docs on the ordering divergence).
fn export_list(heap: &mut Heap, atoms: &AtomTable, module: &Module) -> Result<Eterm> {
    let mut elements = Vec::with_capacity(module.exports.len());
    for export in &module.exports {
        elements.push(callable_pair(heap, atoms, module, export)?);
    }
    build_list(heap, elements)
}

/// The `functions` item: `[{Function, Arity}]` over every function the module defines —
/// including `module_info/0,1` and the local helpers — in IR/code order, which is the
/// oracle's `beam_disasm` order.
fn function_list(heap: &mut Heap, atoms: &AtomTable, module: &Module) -> Result<Eterm> {
    let mut elements = Vec::with_capacity(module.functions.len());
    for function in &module.functions {
        elements.push(callable_pair(heap, atoms, module, function)?);
    }
    build_list(heap, elements)
}

/// A `{FunctionAtom, Arity}` 2-tuple for one export or function entry.
///
/// Both [`Export`] and [`Function`] carry the function name atom and arity; the name is
/// resolved by name into the caller's shared table (see the module docs).
fn callable_pair<T: Callable>(
    heap: &mut Heap,
    atoms: &AtomTable,
    module: &Module,
    entry: &T,
) -> Result<Eterm> {
    let name = module_atom_name(module, entry.name())?;
    let function = atom_term(atoms, name)?;
    let arity = try_make_small(i64::from(entry.arity())).map_err(|_| BeamError::SystemLimit)?;
    heap.tuple(&[function, arity])
}

/// The (name, arity) a callable table entry carries.
trait Callable {
    /// The function-name atom.
    fn name(&self) -> beam_term::Atom;
    /// The arity.
    fn arity(&self) -> u32;
}

impl Callable for Export {
    fn name(&self) -> beam_term::Atom {
        self.function
    }
    fn arity(&self) -> u32 {
        self.arity
    }
}

impl Callable for Function {
    fn name(&self) -> beam_term::Atom {
        self.name
    }
    fn arity(&self) -> u32 {
        self.arity
    }
}

/// Builds a proper list of `elements` (head first), the last cons cell's cdr being `NIL`.
fn build_list(heap: &mut Heap, elements: Vec<Eterm>) -> Result<Eterm> {
    let mut list = NIL;
    for element in elements.into_iter().rev() {
        list = heap.cons(element, list)?;
    }
    Ok(list)
}

/// The `md5` item: the 16-byte `.beam` checksum, as a binary.
///
/// # Errors
///
/// A typed capability gap when the module has no file on disk (loaded from memory) or the
/// file cannot be read, plus [`module_md5`]'s container/`FunT` errors.
fn md5(heap: &mut Heap, code_server: &CodeServer, module: &str) -> Result<Eterm> {
    let Some(path) = code_server.loaded_path(module) else {
        return Err(error::unsupported(
            "get_module_info: the module has no .beam file to checksum",
        ));
    };
    let bytes = std::fs::read(path).map_err(|source| {
        error::unsupported(&format!(
            "get_module_info: cannot read {}: {source}",
            path.display()
        ))
    })?;
    let digest = module_md5(&bytes)?;
    term::make_binary(heap, &digest)
}

/// The `attributes`/`compile` item: the raw `Attr`/`CInf` chunk decoded and remapped.
///
/// An absent chunk is the oracle's `NIL` (there is nothing to render). A present chunk is a
/// `term_to_binary` image (`VERSION_MAGIC 131`): it is decoded onto a scratch heap and
/// atom table, then [`remap`]ped onto the caller's heap so every atom lands in the shared
/// table by name.
fn decode_chunk(heap: &mut Heap, atoms: &AtomTable, raw: Option<&[u8]>) -> Result<Eterm> {
    let Some(bytes) = raw else {
        return Ok(NIL);
    };
    let mut scratch_heap = Heap::new(CHUNK_SCRATCH_WORDS)?;
    let mut scratch_atoms = AtomTable::new();
    let mut decoder = Decoder::new(bytes)?;
    let decoded = decoder.decode(&mut scratch_heap, &mut scratch_atoms)?;
    remap(heap, atoms, &scratch_atoms, decoded)
}

/// Rebuilds `term` on `heap`, resolving every atom **by name** from `scratch` into `atoms`.
///
/// The decoded chunk lives on a scratch heap with its atoms in a scratch table; this walks
/// it structurally and produces an equivalent term whose atoms are the caller's shared
/// indices. Immediates copy directly (small integers, `NIL`, and the internal
/// pid/port/catch immediates); a list, tuple, float, bignum, bitstring or flat map is
/// rebuilt element by element.
///
/// # Errors
///
/// A typed capability gap for an atom whose name is not interned in `atoms`, and for a term
/// shape this walk does not reproduce (a sub-bitstring with a non-zero bit offset, or any
/// block this runtime has no reader for) — never a wrongly rebuilt term.
fn remap(heap: &mut Heap, atoms: &AtomTable, scratch: &AtomTable, term: Eterm) -> Result<Eterm> {
    if is_small(term) || is_nil(term) {
        return Ok(term);
    }
    if is_atom(term) {
        let name = scratch.name(atom_val(term)).ok_or_else(|| {
            error::unsupported("get_module_info: an attribute atom has no name to resolve")
        })?;
        return atom_term(atoms, name);
    }
    // The remaining immediates (internal pid/port ids, the catch marker) are copied
    // verbatim: they encode no atom index.
    if is_immediate(term) {
        return Ok(term);
    }
    if let Ok(arity) = term::tuple_arity(term) {
        let mut elements = Vec::with_capacity(arity);
        for index in 0..arity {
            elements.push(remap(heap, atoms, scratch, term::tuple_element(term, index)?)?);
        }
        return heap.tuple(&elements);
    }
    if beam_term::is_list(term) {
        let head = remap(heap, atoms, scratch, term::cons_head(term)?)?;
        let tail = remap(heap, atoms, scratch, term::cons_tail(term)?)?;
        return heap.cons(head, tail);
    }
    if let Ok(value) = term::float_value(term) {
        return term::make_float(heap, value);
    }
    if let Ok((negative, digits)) = term::big_parts(term) {
        return term::make_big(heap, negative, digits);
    }
    if let Ok((bytes, offset_bits, size_bits)) = term::bitstring_bytes(term) {
        if offset_bits != 0 {
            // A sub-bitstring's payload starts mid-byte; the writer materializes a
            // byte-aligned window, so a non-zero offset is the honest gap rather than a
            // shifted (wrong) bitstring.
            return Err(error::unsupported(
                "get_module_info: a sub-bitstring attribute is not remappable",
            ));
        }
        return term::make_bitstring(heap, bytes, size_bits);
    }
    if let Ok((keys, values)) = term::flatmap_parts(term) {
        let mut pairs = Vec::with_capacity(keys.len());
        for (key, value) in keys.iter().zip(values.iter()) {
            pairs.push((
                remap(heap, atoms, scratch, *key)?,
                remap(heap, atoms, scratch, *value)?,
            ));
        }
        return term::make_flatmap(heap, &pairs);
    }
    Err(error::unsupported(
        "get_module_info: an attribute/compile-info term this runtime cannot remap",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The shared slice fixture (`beam_loader/tests/fixtures/slice.beam`).
    const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    /// A code server with the slice fixture loaded and the runtime's core atoms seeded
    /// (`Runtime::new` seeds them; a bare `CodeServer` does not).
    fn loaded() -> CodeServer {
        let mut cs = CodeServer::new();
        crate::boot::seed_core_atoms(cs.atoms_mut());
        cs.load_path(SLICE_BEAM).expect("slice.beam loads");
        cs
    }

    #[test]
    fn an_unknown_module_is_none() {
        let cs = loaded();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(256).expect("a heap");
        let atoms = cs.atoms();
        assert_eq!(
            bridge
                .module_info(&mut heap, atoms, "no_such_module", "module")
                .expect("the lookup succeeds"),
            None
        );
    }

    #[test]
    fn the_module_item_is_the_module_atom() {
        let cs = loaded();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(256).expect("a heap");
        let atoms = cs.atoms();
        let value = bridge
            .module_info(&mut heap, atoms, "slice", "module")
            .expect("module")
            .expect("slice is loaded");
        assert!(is_atom(value));
        assert_eq!(atoms.name(atom_val(value)), Some("slice"));
    }

    #[test]
    fn the_native_item_is_the_false_atom() {
        let cs = loaded();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(256).expect("a heap");
        let atoms = cs.atoms();
        let value = bridge
            .module_info(&mut heap, atoms, "slice", "native")
            .expect("native")
            .expect("slice is loaded");
        assert!(is_atom(value));
        assert_eq!(atoms.name(atom_val(value)), Some("false"));
    }

    #[test]
    fn the_empty_items_are_nil() {
        let cs = loaded();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(256).expect("a heap");
        let atoms = cs.atoms();
        for item in ["nifs", "native_addresses"] {
            let value = bridge
                .module_info(&mut heap, atoms, "slice", item)
                .expect("the item")
                .expect("slice is loaded");
            assert_eq!(value, NIL, "`{item}` must be the empty list");
        }
    }

    #[test]
    fn the_functions_item_lists_every_function_with_its_arity() {
        let cs = loaded();
        let module = cs.module("slice").expect("slice is loaded");
        let expected: Vec<(String, u32)> = module
            .functions
            .iter()
            .map(|f| {
                (
                    module.atom_name(f.name).expect("a function name").to_string(),
                    f.arity,
                )
            })
            .collect();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(4096).expect("a heap");
        let atoms = cs.atoms();
        let value = bridge
            .module_info(&mut heap, atoms, "slice", "functions")
            .expect("functions")
            .expect("slice is loaded");
        // Walk the list into (name, arity) pairs.
        let mut got = Vec::new();
        let mut cursor = value;
        while !is_nil(cursor) {
            let pair = term::cons_head(cursor).expect("a cons cell");
            let name = atoms
                .name(atom_val(term::tuple_element(pair, 0).expect("head name")))
                .expect("a name")
                .to_string();
            let arity = beam_term::signed_value(term::tuple_element(pair, 1).expect("arity"));
            got.push((name, arity as u32));
            cursor = term::cons_tail(cursor).expect("a cons tail");
        }
        assert!(!got.is_empty(), "the fixture defines functions");
        assert_eq!(got, expected, "functions are listed in IR order with their arity");
    }

    /// The `attributes` item is the decoded `Attr` chunk, remapped onto the caller's
    /// heap/table: for the slice fixture the oracle reports `[{vsn,[<bignum>]}]`, so its
    /// key atom must resolve to `vsn` in the **caller's** table.
    #[test]
    fn the_attributes_item_decodes_and_remaps() {
        let cs = loaded();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(4096).expect("a heap");
        let atoms = cs.atoms();
        let value = bridge
            .module_info(&mut heap, atoms, "slice", "attributes")
            .expect("attributes")
            .expect("slice is loaded");
        assert!(beam_term::is_list(value), "the Attr chunk is a list");
        let pair = term::cons_head(value).expect("a `{Key, Value}` tuple");
        let key = term::tuple_element(pair, 0).expect("the key");
        assert_eq!(
            atoms.name(atom_val(key)),
            Some("vsn"),
            "the attribute key must resolve in the caller's table"
        );
    }

    /// The `compile` item is the decoded `CInf` chunk: for the slice fixture the oracle
    /// reports `[{version,"10.0.6"}]`, so its key atom must resolve to `version`.
    #[test]
    fn the_compile_item_decodes_and_remaps() {
        let cs = loaded();
        let mut bridge = ModuleInfoBridge { code_server: &cs };
        let mut heap = Heap::new(4096).expect("a heap");
        let atoms = cs.atoms();
        let value = bridge
            .module_info(&mut heap, atoms, "slice", "compile")
            .expect("compile")
            .expect("slice is loaded");
        assert!(beam_term::is_list(value), "the CInf chunk is a list");
        let pair = term::cons_head(value).expect("a `{Key, Value}` tuple");
        let key = term::tuple_element(pair, 0).expect("the key");
        assert_eq!(atoms.name(atom_val(key)), Some("version"));
    }
}
