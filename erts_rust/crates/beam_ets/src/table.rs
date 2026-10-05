//! The [`Table`] handle, its identifiers and the internal `Interior` dispatch
//! seam.
//!
//! [`Table`] owns the table metadata (id, kind, keypos, access, owner), a
//! table-owned [`TermStore`] (the copy boundary) and one interior behind the
//! private [`Interior`] trait. The interior shape is *not* externally pinned
//! (workspace `CONTEXT.md` §2): a hash index and an ordered index implement the
//! same trait, so a future CHAMP/HAMT/B-tree interior slots in without touching
//! this dispatch or any dependent.

use core::fmt;

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::*;

use crate::info::TableInfo;
use crate::kind::{Access, TableKind, TableOptions};
use crate::select::{MatchSpec, SelectPredicate};
use crate::store::TermStore;

/// The runtime identifier of a table (`ets:new/2` returns one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(u64);

impl TableId {
    /// Wrap a raw identifier.
    pub fn new(raw: u64) -> TableId {
        unimplemented!("M1: implementation pending")
    }

    /// The raw identifier value.
    pub fn raw(self) -> u64 {
        unimplemented!("M1: implementation pending")
    }

    /// Allocate the next process-global table identifier.
    pub fn next() -> TableId {
        unimplemented!("M1: implementation pending")
    }
}

impl fmt::Display for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unimplemented!("M1: implementation pending")
    }
}

/// The owning process of a table.
///
/// M1 **skeleton**: access is not yet enforced by owner (see the crate
/// `CONTEXT.md`, Known Issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnerId(u64);

impl OwnerId {
    /// Wrap a raw owner identifier.
    pub fn new(raw: u64) -> OwnerId {
        unimplemented!("M1: implementation pending")
    }

    /// The raw owner identifier value.
    pub fn raw(self) -> u64 {
        unimplemented!("M1: implementation pending")
    }
}

/// The internal dispatch seam implemented by the crate's interiors
/// (`HashInterior`, `OrderedSetInterior`).
///
/// Private: **not** part of the cross-crate API. [`Table`] derives the key from
/// the object (`crate::key::key_of`) and passes both to the interior, so the two
/// interiors can share one dispatch surface despite different shapes. All term
/// ordering/hashing is delegated to `beam_term`.
pub(crate) trait Interior {
    /// Insert `object` under `key`, applying the kind's duplicate rules
    /// (replace for `set`/`ordered_set`).
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()>;

    /// Insert `object` under `key` only if `key` is absent; returns whether the
    /// object was inserted.
    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool>;

    /// Remove every object stored under `key`.
    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()>;

    /// Remove the single object that equals `object` within `key`'s objects.
    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()>;

    /// All objects stored under `key` (0 or 1 for `set`/`ordered_set`).
    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm>;

    /// Whether any object is stored under `key`.
    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool;

    /// The `pos`-th element (0-based) of the single object stored under `key`;
    /// `badarg` when `key` is absent.
    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm>;

    /// The total number of stored objects (not keys, for the multi kinds).
    fn size(&self) -> usize;

    /// Every stored object, ordered for `ordered_set`.
    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_>;
}

/// An ETS table handle: metadata plus table-owned storage and one interior.
pub struct Table {
    /// The table identifier.
    id: TableId,
    /// The parsed `ets:new/2` options.
    opts: TableOptions,
    /// The owning process, if known.
    owner: Option<OwnerId>,
    /// The table-owned term storage (the copy boundary).
    store: TermStore,
    /// The interior index dispatching the table ops.
    interior: Box<dyn Interior>,
}

impl Table {
    /// A new table with the given identifier and options, allocating through the
    /// registered ETS allocator.
    pub fn new(id: TableId, opts: TableOptions) -> Result<Table> {
        unimplemented!("M1: implementation pending")
    }

    /// A new table allocating its term storage through `alloc`.
    pub fn with_allocator(id: TableId, opts: TableOptions, alloc: AllocRef) -> Result<Table> {
        unimplemented!("M1: implementation pending")
    }

    /// The table identifier.
    pub fn id(&self) -> TableId {
        unimplemented!("M1: implementation pending")
    }

    /// The table kind.
    pub fn kind(&self) -> TableKind {
        unimplemented!("M1: implementation pending")
    }

    /// The registered table name, if any (`Some` iff named).
    pub fn name(&self) -> Option<Atom> {
        unimplemented!("M1: implementation pending")
    }

    /// The 1-based key position.
    pub fn keypos(&self) -> usize {
        unimplemented!("M1: implementation pending")
    }

    /// The access mode.
    pub fn access(&self) -> Access {
        unimplemented!("M1: implementation pending")
    }

    /// The owning process, if known.
    pub fn owner(&self) -> Option<OwnerId> {
        unimplemented!("M1: implementation pending")
    }

    /// Set (or clear) the owning process.
    pub fn set_owner(&mut self, owner: Option<OwnerId>) {
        unimplemented!("M1: implementation pending")
    }

    /// A snapshot of the table's introspection fields.
    pub fn info(&self) -> TableInfo {
        unimplemented!("M1: implementation pending")
    }

    /// Insert `object` (the whole tuple; the key is derived from `keypos`).
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    /// Insert `object` only if its key is absent; returns whether it was
    /// inserted (`false` when the key already exists).
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        unimplemented!("M1: implementation pending")
    }

    /// Insert every object of `objects` (`ets:insert/2` over a list).
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    /// Delete every object stored under `key` (`ets:delete/2`).
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    /// Delete the single object matching `object` (`ets:delete_object/2`).
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    /// Delete every object in the table (`ets:delete_all_objects/1`).
    pub fn delete_all_objects(&mut self) {
        unimplemented!("M1: implementation pending")
    }

    /// The objects stored under `key` (0 or 1 for `set`/`ordered_set`).
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    /// Whether `key` is present.
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        unimplemented!("M1: implementation pending")
    }

    /// The `pos`-th element (1-based) of the single object stored under `key`;
    /// `badarg` when `key` is absent or `pos` is out of range.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    /// The number of objects in the table (`ets:size/1`).
    pub fn size(&self) -> usize {
        unimplemented!("M1: implementation pending")
    }

    /// Whether the table holds no objects.
    pub fn is_empty(&self) -> bool {
        unimplemented!("M1: implementation pending")
    }

    /// Every object in the table (ordered for `ordered_set`).
    pub fn tab2list(&self) -> Vec<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    /// Every object whose element slice `pred` accepts (the M1 select
    /// primitive).
    pub fn select_all(&self, pred: &mut SelectPredicate<'_>) -> Vec<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    /// The objects a compiled match spec selects (the M1 subset).
    pub fn select(&self, spec: &MatchSpec, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        unimplemented!("M1: implementation pending")
    }
}
