//! Table introspection — the `ets:info/1` fields.
//!
//! [`TableInfo`] is the heap-free snapshot [`crate::Table::info`] returns; its
//! [`TableInfo::entries`] renders the `{Key, Value}` proplist pairs the `info`
//! BIF turns into the `ets:info/1` list. No term construction happens here — the
//! pairs are `(Atom, Eterm)` and interning the field-name atoms is the only use
//! of the [`AtomTable`].

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
    /// Whether the table was created with `named_table`.
    pub named_table: bool,
    /// Whether the table stores compressed terms.
    pub compressed: bool,
    /// Whether concurrent writers are enabled.
    pub write_concurrency: bool,
    /// Whether concurrent readers are enabled.
    pub read_concurrency: bool,
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
    /// The `{Key, Value}` proplist pairs of the table's `ets:info/1` fields
    /// (`id`/`name`/`type`/`keypos`/`protection`/`size`/`memory`/`owner`/…).
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

        vec![
            pair(atoms, "id", make_small(self.id.raw() as i64)),
            pair(atoms, "name", name),
            pair(atoms, "named_table", named_table),
            pair(atoms, "type", kind),
            pair(atoms, "keypos", make_small(self.keypos as i64)),
            pair(atoms, "protection", protection),
            pair(atoms, "size", make_small(self.size as i64)),
            pair(atoms, "memory", make_small(self.memory as i64)),
            pair(atoms, "owner", owner),
            pair(atoms, "compressed", compressed),
            pair(atoms, "write_concurrency", write_concurrency),
            pair(atoms, "read_concurrency", read_concurrency),
        ]
    }
}
