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

//! Table introspection — the `ets:info/1` fields.
//!
//! [`TableInfo`] is the heap-free snapshot [`crate::Table::info`] returns; its
//! [`TableInfo::entries`] renders the `{Key, Value}` proplist pairs the `info`
//! BIF turns into the `ets:info/1` list. No term construction happens here — the
//! pairs are `(Atom, Eterm)` and interning the field-name atoms is the only use
//! of the [`AtomTable`].
//!
//! The proplist carries the oracle's **15** fields, in the oracle's order
//! (`id`, `decentralized_counters`, `read_concurrency`, `write_concurrency`,
//! `compressed`, `memory`, `owner`, `heir`, `name`, `size`, `node`, `named_table`,
//! `type`, `keypos`, `protection` — the reverse of the C `fields[]` array in
//! `erl_db.c`), for every kind, named or not. Three of them are fixed by the
//! runtime shape rather than by the table:
//!
//! * `heir` is the atom `none` when no heir is installed and the atom
//!   `undefined` when one is (a **documented pid gap**: the oracle reports the
//!   heir *pid*, which this stage cannot represent — see [`TableInfo::heir`]);
//! * `node` is always the atom `nonode@nohost` (the runtime is never
//!   distributed, matching the oracle's `erl -noshell` with no `-sname`);
//! * `decentralized_counters` is the value the oracle derives at creation time —
//!   [`TableOptions::decentralized_counters`](crate::TableOptions::decentralized_counters)
//!   owns the measured rule (the explicit `{decentralized_counters, Bool}`
//!   option wins over the kind's default, and the effective flag is gated on
//!   `write_concurrency`). [`TableInfo`] carries the resolved value, so nothing
//!   is recomputed here.
//!
//! `owner` is a **documented gap**: the oracle reports the owner *pid*, which
//! this stage cannot represent, so it is rendered as the atom `undefined`.

use beam_term::prelude::*;

use crate::kind::{Access, TableKind};
use crate::table::{OwnerId, TableId};

/// A snapshot of a table's introspection fields (`ets:info/1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    /// The table identifier.
    pub id: TableId,
    /// The registered name, if any.
    pub name: Option<Atom>,
    /// The table kind (`type`).
    pub kind: TableKind,
    /// The 1-based key position.
    pub keypos: usize,
    /// The access mode (`protection`).
    pub access: Access,
    /// The number of objects in the table.
    pub size: usize,
    /// The number of words allocated to the table (`memory`).
    pub memory: usize,
    /// The owning process, if known.
    pub owner: Option<OwnerId>,
    /// The heir installed with `ets:setopts/2` (`{heir, Pid, _}`), if any.
    ///
    /// The introspectable heir is **only the pid**: the gift data is never
    /// reported by `ets:info/2` (the C `ets_setopts_2` keeps it out of the info
    /// fields). As with [`owner`](Self::owner) this crate has no pid term, so
    /// [`TableInfo::entries`] renders a set heir as the atom `undefined`.
    pub heir: Option<OwnerId>,
    /// Whether the table was created with `named_table`.
    pub named_table: bool,
    /// Whether the table stores compressed terms.
    pub compressed: bool,
    /// Whether concurrent writers are enabled.
    pub write_concurrency: bool,
    /// Whether concurrent readers are enabled.
    pub read_concurrency: bool,
    /// The `decentralized_counters` value the oracle reports for the table.
    ///
    /// Resolved once, at creation time, by
    /// [`TableOptions::decentralized_counters`](crate::TableOptions::decentralized_counters)
    /// (the measured `db_new` rule: the explicit `{decentralized_counters, Bool}`
    /// option wins over the kind's default — `true` only for `ordered_set` — and
    /// the result is gated on the effective `write_concurrency`). It is carried
    /// here rather than derived from [`kind`](Self::kind)/
    /// [`write_concurrency`](Self::write_concurrency) because the explicit option
    /// is not recoverable from those two.
    pub decentralized_counters: bool,
}

/// A `(field-atom, value)` pair, with the field name interned in `atoms`.
fn pair(atoms: &mut AtomTable, field: &str, value: Eterm) -> (Atom, Eterm) {
    (atoms.intern(field), value)
}

/// The term of the atom `name` (interning it in `atoms`).
fn atom_term(atoms: &mut AtomTable, name: &str) -> Eterm {
    make_atom(atoms.intern(name))
}

/// The `true`/`false` atom term for `value`.
fn bool_term(atoms: &mut AtomTable, value: bool) -> Eterm {
    atom_term(atoms, if value { "true" } else { "false" })
}

impl TableInfo {
    /// The `{Key, Value}` proplist pairs of the table's `ets:info/1` fields, in
    /// the oracle's order: `id`, `decentralized_counters`, `read_concurrency`,
    /// `write_concurrency`, `compressed`, `memory`, `owner`, `heir`, `name`,
    /// `size`, `node`, `named_table`, `type`, `keypos`, `protection`.
    ///
    /// `heir` is the atom `none` when unset and `undefined` when an heir is
    /// installed (the documented pid gap — see [`owner`](Self::owner) and
    /// [`heir`](Self::heir)); `decentralized_counters` is the value resolved by
    /// [`TableOptions::decentralized_counters`](crate::TableOptions::decentralized_counters)
    /// at creation time (carried on this snapshot, not recomputed); `owner` is
    /// the atom `undefined` (the documented pid gap).
    ///
    /// Heap-free: [`atoms`](AtomTable) is used only to intern the field-name
    /// atoms; values are returned as `(Atom, Eterm)` pairs.
    pub fn entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)> {
        let name = match self.name {
            Some(atom) => make_atom(atom),
            // An unnamed table reports `undefined`.
            None => atom_term(atoms, "undefined"),
        };
        let kind = atom_term(atoms, self.kind.as_atom_name());
        let protection = atom_term(atoms, self.access.as_atom_name());
        // M1 has no pid terms to report, so the owner is rendered as `undefined`.
        let owner = atom_term(atoms, "undefined");
        let compressed = bool_term(atoms, self.compressed);
        let write_concurrency = bool_term(atoms, self.write_concurrency);
        let read_concurrency = bool_term(atoms, self.read_concurrency);
        let named_table = bool_term(atoms, self.named_table);
        // The oracle reports `tb->common.counters.is_decentralized`, resolved by
        // `db_new` from the fine-locked flag and the decentralized-counters flag
        // (the explicit option wins over the kind's default).
        let decentralized_counters = bool_term(atoms, self.decentralized_counters);
        // A fresh table's heir is the atom `none`; an installed heir is a pid term
        // this stage cannot represent (the documented gap), so it is `undefined`.
        let heir = match self.heir {
            None => atom_term(atoms, "none"),
            Some(_) => atom_term(atoms, "undefined"),
        };
        // The runtime is never distributed.
        let node = atom_term(atoms, "nonode@nohost");
        vec![
            pair(atoms, "id", make_small(self.id.raw() as i64)),
            pair(atoms, "decentralized_counters", decentralized_counters),
            pair(atoms, "read_concurrency", read_concurrency),
            pair(atoms, "write_concurrency", write_concurrency),
            pair(atoms, "compressed", compressed),
            pair(atoms, "memory", make_small(self.memory as i64)),
            pair(atoms, "owner", owner),
            pair(atoms, "heir", heir),
            pair(atoms, "name", name),
            pair(atoms, "size", make_small(self.size as i64)),
            pair(atoms, "node", node),
            pair(atoms, "named_table", named_table),
            pair(atoms, "type", kind),
            pair(atoms, "keypos", make_small(self.keypos as i64)),
            pair(atoms, "protection", protection),
        ]
    }
}
