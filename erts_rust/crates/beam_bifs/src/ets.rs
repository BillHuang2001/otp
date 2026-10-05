//! The `ets:*` bridge: a [`TableRegistry`] mapping opaque table handles to
//! `beam_ets::Table`s.
//!
//! `beam_ets` owns the table semantics; this module owns the M1 runtime side — a
//! map from a handle to a live table, plus the `ets:new/insert/lookup/delete`
//! BIF entry point (implemented in a follow-up step).
//!
//! # The M1 table handle
//!
//! Real ERTS identifies a table by a `tid()`. Milestone-1 has no `make_ref` in
//! `beam_term`, so [`crate::access::make_ref_thing`] builds the frozen 4-word ref
//! thing `[make_header(3, REF_SUBTAG), 0, 0, id]` and its numeric `id` is the
//! registry key (mirroring `beam_process::MonitorRef::to_eterm`). When `beam_term`
//! gains a proper `make_ref`, only the handle constructor changes (see the crate
//! `CONTEXT.md`, Design Decisions / Known Issues).

use std::collections::HashMap;

use beam_ets::Table;

use crate::registry::BifRegistry;

/// The runtime's ETS tables, keyed by the numeric id of the on-heap ref thing
/// built by [`crate::access::make_ref_thing`].
#[derive(Default)]
pub struct TableRegistry {
    tables: HashMap<u64, Table>,
}

impl TableRegistry {
    /// An empty registry.
    pub fn new() -> TableRegistry {
        TableRegistry {
            tables: HashMap::new(),
        }
    }

    /// Inserts `table` under `handle`, returning the previous table if the handle
    /// was already present.
    pub fn insert(&mut self, handle: u64, table: Table) -> Option<Table> {
        self.tables.insert(handle, table)
    }

    /// The table stored under `handle`, if any.
    pub fn get(&self, handle: u64) -> Option<&Table> {
        self.tables.get(&handle)
    }

    /// The table stored under `handle`, mutably, if any.
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Table> {
        self.tables.get_mut(&handle)
    }

    /// Whether `handle` names a table.
    pub fn contains(&self, handle: u64) -> bool {
        self.tables.contains_key(&handle)
    }

    /// Removes and returns the table stored under `handle`, if any.
    pub fn remove(&mut self, handle: u64) -> Option<Table> {
        self.tables.remove(&handle)
    }

    /// The number of live tables.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Whether the registry holds no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// Registers this module's BIFs (`ets:new/insert/lookup/delete`), implemented in a
/// follow-up step.
pub(crate) fn register(_reg: &mut BifRegistry) {}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_ets::{TableId, TableOptions};

    fn table(id: u64) -> Table {
        Table::new(TableId::new(id), TableOptions::new()).expect("test table")
    }

    #[test]
    fn insert_get_and_remove_round_trip() {
        let mut reg = TableRegistry::new();
        assert!(reg.is_empty());

        assert!(reg.insert(7, table(1)).is_none());
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(7));
        assert!(reg.get(7).is_some());
        assert!(reg.get_mut(7).is_some());
        assert!(reg.get(8).is_none());

        assert!(reg.remove(7).is_some());
        assert!(reg.is_empty());
        assert!(reg.remove(7).is_none());
    }

    #[test]
    fn insert_replaces_and_returns_the_previous_table() {
        let mut reg = TableRegistry::new();
        reg.insert(1, table(1));
        assert!(reg.insert(1, table(2)).is_some());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn default_is_empty() {
        assert!(TableRegistry::default().is_empty());
    }
}
