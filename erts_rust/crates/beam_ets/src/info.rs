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
    /// The owning process, if known.
    pub owner: Option<OwnerId>,
    /// Whether the table stores compressed terms.
    pub compressed: bool,
    /// Whether concurrent writers are enabled.
    pub write_concurrency: bool,
    /// Whether concurrent readers are enabled.
    pub read_concurrency: bool,
}

impl TableInfo {
    /// The `{Key, Value}` proplist pairs of the table's `ets:info/1` fields
    /// (`id`/`name`/`type`/`keypos`/`protection`/`size`/`owner`/…).
    ///
    /// Heap-free: [`atoms`](AtomTable) is used only to intern the field-name
    /// atoms; values are returned as `(Atom, Eterm)` pairs.
    pub fn entries(&self, atoms: &mut AtomTable) -> Vec<(Atom, Eterm)> {
        unimplemented!("M1: implementation pending")
    }
}
