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
//! The **initial atom index space** of the runtime: `erts/emulator/beam/atom.names`.
//!
//! ERTS assigns an atom its index in the order the atom is *first interned*, and the emulator
//! interns `atom.names` (its own C-side atom-name table) before it loads a single module; every
//! later atom — a module's `AtU8` chunk, a `list_to_atom/1`, a `binary_to_term/1` name — is
//! appended in load/usage order. Atom comparison (`<`, `lists:sort/1`) is by **name** since
//! OTP 26, but the *storage* order of a flat map's key tuple, `maps:keys/1`, a `~p`-rendered map
//! and the key sequence of `term_to_binary/1`'s `MAP_EXT` all follow the atom **index**. So the
//! seed below is what makes this runtime's observable map/printing/ETF ordering agree with the
//! oracle; without it every index is whatever `CORE_ATOMS` (an unrelated hand-ordered list) and
//! the boot-time vocabulary happen to be, and those orders diverge.
//!
//! # Why a compile-time `include_str!` across the tree
//!
//! `atom.names` is the committed *source data* the legacy C emulator generates its atom table
//! from (`erts/emulator/beam/atom.names`, parsed by the emulator's build), i.e. the file that
//! defines ERTS' atom index order. It is embedded with [`include_str!`] rather than read at run
//! time so the seed is a build-time constant: a `beam_rt` binary carries its atom order with it
//! and needs no installed (or present-on-disk) `$ERL_TOP` at run time, and a missing tree can
//! never silently produce a partially-seeded table. It is the workspace's **only** cross-tree
//! `include_str!`; every other runtime input is either a Rust constant or a file the runtime
//! finds through its own code path.
//!
//! The parse is [`beam_term::AtomTable::seed_from_atom_names`]'s (the grammar lives there), and
//! interning is idempotent, so a name that the file lists twice keeps its first index.

/// The committed `atom.names` source, embedded at compile time — see the module docs.
pub const ATOM_NAMES_SRC: &str = include_str!("../../../../erts/emulator/beam/atom.names");

/// Interns [`ATOM_NAMES_SRC`] into `atom_table` in **file order**.
///
/// The table is expected to be fresh (or to hold only names the file already lists): an index
/// this call assigns is the atom's position in `atom.names`' first-appearance order, which is the
/// index ERTS gives it. Calling it on a table that already holds names the file does *not* list
/// cannot reproduce ERTS' order for those names — every caller in this crate seeds before it
/// interns anything else (see [`crate::codeserver::CodeServer::new`]).
///
/// The `TermResult` is unwrapped with an [`expect`] because the source is a **compile-time
/// constant**: a malformed line would be a build artifact defect, not a run-time condition, and
/// [`the_embedded_source_parses`] pins that it parses (a `let _ = …` here would hide exactly the
/// failure this runtime must never have).
pub fn seed(atom_table: &mut beam_term::AtomTable) {
    atom_table
        .seed_from_atom_names(ATOM_NAMES_SRC)
        .expect("erts/emulator/beam/atom.names is a committed, well-formed build input");
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{Atom, AtomTable};

    /// The golden `maps:keys/1` row this seed exists to reproduce: the oracle's
    /// `maps:keys(#{ok=>1,error=>2,undefined=>3,true=>4,false=>5,nil=>6,infinity=>7,timeout=>8})`
    /// is `[false,true,infinity,timeout,error,undefined,nil,ok]` — atom.names order, which is
    /// neither alphabetical nor the old `CORE_ATOMS` order.
    const GOLDEN_ORDER: [&str; 8] = [
        "false", "true", "infinity", "timeout", "error", "undefined", "nil", "ok",
    ];

    /// The seed cannot fail: the embedded source parses. A failure here is a defect in the
    /// committed `atom.names` (or in `beam_term`'s parser), never a run-time condition.
    #[test]
    fn the_embedded_source_parses() {
        let mut table = AtomTable::new();
        table
            .seed_from_atom_names(ATOM_NAMES_SRC)
            .expect("atom.names must parse");
        assert!(
            table.len() > 700,
            "atom.names is the emulator's whole C-side name table, not a handful: {}",
            table.len()
        );
    }

    /// The first names land at indices `0..N` in file order.
    #[test]
    fn the_file_order_is_the_index_order() {
        let mut table = AtomTable::new();
        seed(&mut table);
        // The file's first line is `atom false true` and its head is
        // `false true '_' nonode@nohost '$end_of_table' '' infinity timeout normal call …`.
        assert_eq!(table.lookup("false").map(Atom::raw), Some(0), "the first name in the file");
        assert_eq!(table.lookup("true").map(Atom::raw), Some(1));
        assert_eq!(table.lookup("_").map(Atom::raw), Some(2));
        assert_eq!(table.lookup("nonode@nohost").map(Atom::raw), Some(3));
        assert_eq!(table.lookup("$end_of_table").map(Atom::raw), Some(4));
        assert_eq!(table.lookup("").map(Atom::raw), Some(5), "the empty atom is in the file");
        // The first eight of the golden row, strictly ascending by index.
        let indices: Vec<_> = GOLDEN_ORDER
            .iter()
            .map(|name| {
                table
                    .lookup(name)
                    .unwrap_or_else(|| panic!("{name} must be seeded"))
                    .raw()
            })
            .collect();
        assert!(
            indices.windows(2).all(|pair| pair[0] < pair[1]),
            "the golden row must be in ascending index order: {indices:?}"
        );
    }

    /// The seed only *appends* a name the file lacks: an unknown name gets an index at/after the
    /// seeded run.
    #[test]
    fn a_name_absent_from_the_file_is_appended_after_the_seed() {
        let mut table = AtomTable::new();
        seed(&mut table);
        let end = table.len();
        let idx = table.intern("a_name_no_atom_names_line_contains").raw() as usize;
        assert!(
            idx >= end,
            "an unlisted name is appended, never inserted: {idx} < {end}"
        );
        assert_eq!(table.lookup("false").map(Atom::raw), Some(0), "the seed's indices are unchanged");
    }

    /// Seeding twice changes nothing — neither the length nor any index.
    #[test]
    fn seeding_is_idempotent() {
        let mut table = AtomTable::new();
        seed(&mut table);
        let once: Vec<Option<u32>> = GOLDEN_ORDER
            .iter()
            .map(|name| table.lookup(name).map(Atom::raw))
            .collect();
        let len = table.len();
        seed(&mut table);
        assert_eq!(table.len(), len, "a second seed interns no new name");
        let twice: Vec<Option<u32>> = GOLDEN_ORDER
            .iter()
            .map(|name| table.lookup(name).map(Atom::raw))
            .collect();
        assert_eq!(once, twice, "every index is stable across a second seed");
    }

    /// A name the file lists twice occupies **one** index (its first appearance).
    ///
    /// `atom.names` really does repeat names (`infinity` is declared on its own line and again
    /// with `timeout`/`normal`/`call`/`return`), so this is not a hypothetical.
    #[test]
    fn a_duplicated_name_occupies_one_index() {
        let mut table = AtomTable::new();
        seed(&mut table);
        let first = table.lookup("infinity").expect("seeded").raw();
        let len = table.len();
        assert!(
            (table.intern("infinity").raw() as usize) >= len,
            "no new index for the repeat"
        );
        assert_eq!(table.lookup("infinity").map(Atom::raw), Some(first));
        // `infinity` is declared on its own line *and* again in `atom infinity timeout normal
        // call return`, so the next name in that line proves the repeat was folded rather than
        // appended.
        assert!(first < table.lookup("timeout").expect("seeded").raw());
    }
}
