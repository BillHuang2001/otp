//! The [`Table`] handle, its identifiers and the internal `Interior` dispatch
//! seam.
//!
//! [`Table`] owns the table metadata (id, kind, keypos, access, owner), a
//! table-owned [`TermStore`] (the copy boundary) and one interior behind the
//! private `Interior` trait. The interior shape is *not* externally pinned
//! (workspace `CONTEXT.md` §2): a hash index and an ordered index implement the
//! same trait, so a future CHAMP/HAMT/B-tree interior slots in without touching
//! this dispatch or any dependent.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::hash_interior::HashInterior;
use crate::info::TableInfo;
use crate::key::{key_of, object_elements};
use crate::kind::{Access, TableKind, TableOptions};
use crate::ordered_interior::OrderedSetInterior;
use crate::select::{
    MatchBody, MatchBodyElement, MatchElement, MatchGuard, MatchPattern, MatchSpec, SelectPredicate,
};
use crate::store::TermStore;

/// The process-global source of table identifiers (`ets:new/2` hands one out per
/// table). Monotonic and never reused; the identifier itself is opaque to
/// callers.
static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

/// The runtime identifier of a table (`ets:new/2` returns one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(u64);

impl TableId {
    /// Wrap a raw identifier.
    pub fn new(raw: u64) -> TableId {
        TableId(raw)
    }

    /// The raw identifier value.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Allocate the next process-global table identifier.
    pub fn next() -> TableId {
        TableId(NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
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
        OwnerId(raw)
    }

    /// The raw owner identifier value.
    pub fn raw(self) -> u64 {
        self.0
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

/// Build the interior matching `kind`: the hash index for `set`/`bag`/
/// `duplicate_bag`, the key-sorted index for `ordered_set`.
fn build_interior(kind: TableKind) -> Box<dyn Interior> {
    match kind {
        TableKind::Set | TableKind::Bag | TableKind::DuplicateBag => {
            Box::new(HashInterior::new(kind))
        }
        TableKind::OrderedSet => Box::new(OrderedSetInterior::new()),
    }
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
        Table::build(id, opts, TermStore::with_default_allocator())
    }

    /// A new table allocating its term storage through `alloc`.
    pub fn with_allocator(id: TableId, opts: TableOptions, alloc: AllocRef) -> Result<Table> {
        Table::build(id, opts, TermStore::new(alloc))
    }

    /// The shared constructor: assemble the metadata, the store and the interior
    /// for `opts`.
    fn build(id: TableId, opts: TableOptions, store: TermStore) -> Result<Table> {
        let interior = build_interior(opts.kind());
        Ok(Table {
            id,
            opts,
            owner: None,
            store,
            interior,
        })
    }

    /// The table identifier.
    pub fn id(&self) -> TableId {
        self.id
    }

    /// The table kind.
    pub fn kind(&self) -> TableKind {
        self.opts.kind()
    }

    /// The registered table name, if any (`Some` iff named).
    pub fn name(&self) -> Option<Atom> {
        self.opts.name()
    }

    /// The 1-based key position.
    pub fn keypos(&self) -> usize {
        self.opts.keypos()
    }

    /// The access mode.
    pub fn access(&self) -> Access {
        self.opts.access()
    }

    /// The owning process, if known.
    pub fn owner(&self) -> Option<OwnerId> {
        self.owner
    }

    /// Set (or clear) the owning process.
    pub fn set_owner(&mut self, owner: Option<OwnerId>) {
        self.owner = owner;
    }

    /// A snapshot of the table's introspection fields.
    pub fn info(&self) -> TableInfo {
        TableInfo {
            id: self.id,
            name: self.opts.name(),
            kind: self.opts.kind(),
            keypos: self.opts.keypos(),
            access: self.opts.access(),
            size: self.interior.size(),
            owner: self.owner,
            compressed: self.opts.is_compressed(),
            write_concurrency: self.opts.write_concurrency(),
            read_concurrency: self.opts.read_concurrency(),
        }
    }

    /// Insert `object` (the whole tuple; the key is derived from `keypos`).
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let keypos = self.opts.keypos();
        // Validate the caller's object before copying: a non-tuple / too-short
        // object is `badarg` and allocates nothing.
        key_of(object, keypos)?;
        // Deep-copy into table-owned storage (the A.1 copy boundary), then derive
        // the *stored* key from the copy so the interior never aliases a caller
        // heap.
        let stored = self.store.copy(object)?;
        let stored_key = key_of(stored, keypos)?;
        self.interior.insert(stored_key, stored, atoms)
    }

    /// Insert `object` only if its key is absent; returns whether it was
    /// inserted (`false` when the key already exists).
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let keypos = self.opts.keypos();
        key_of(object, keypos)?;
        let stored = self.store.copy(object)?;
        let stored_key = key_of(stored, keypos)?;
        self.interior.insert_new(stored_key, stored, atoms)
    }

    /// Insert every object of `objects` (`ets:insert/2` over a list).
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()> {
        for &object in objects {
            self.insert(object, atoms)?;
        }
        Ok(())
    }

    /// Delete every object stored under `key` (`ets:delete/2`).
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        self.interior.delete(key, atoms)
    }

    /// Delete the single object matching `object` (`ets:delete_object/2`).
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        // The key is only used to locate the entry; the object itself is compared.
        let key = key_of(object, self.opts.keypos())?;
        self.interior.delete_object(key, object, atoms)
    }

    /// Delete every object in the table (`ets:delete_all_objects/1`).
    pub fn delete_all_objects(&mut self) {
        // A fresh interior of the same kind is empty; the `TermStore` keeps its
        // blocks (M1 does not reclaim term storage — see the crate `CONTEXT.md`,
        // Known Issues).
        self.interior = build_interior(self.opts.kind());
    }

    /// The objects stored under `key` (0 or 1 for `set`/`ordered_set`).
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        self.interior.lookup(key, atoms)
    }

    /// Whether `key` is present.
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        self.interior.member(key, atoms)
    }

    /// The `pos`-th element (1-based) of the single object stored under `key`;
    /// `badarg` when `key` is absent or `pos` is out of range.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        // The public position is 1-based; `0` is `badarg` (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        self.interior.lookup_element(key, pos, atoms)
    }

    /// The number of objects in the table (`ets:size/1`).
    pub fn size(&self) -> usize {
        self.interior.size()
    }

    /// Whether the table holds no objects.
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Every object in the table (ordered for `ordered_set`).
    pub fn tab2list(&self) -> Vec<Eterm> {
        self.interior.iter_objects().collect()
    }

    /// Every object whose element slice `pred` accepts (the M1 select
    /// primitive).
    pub fn select_all(&self, pred: &mut SelectPredicate<'_>) -> Vec<Eterm> {
        let mut selected = Vec::new();
        for object in self.interior.iter_objects() {
            // SAFETY: every object in a table is a tuple validated by `key_of` on
            // insert (and the deep copy preserves that), so its header word is a
            // live, valid arity value; the returned slice borrows table storage,
            // which outlives this borrow.
            let elements = unsafe { object_elements(object) };
            if pred(elements) {
                selected.push(object);
            }
        }
        selected
    }

    /// The objects a compiled match spec selects (the M1 subset).
    pub fn select(&self, spec: &MatchSpec, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let mut selected = Vec::new();
        for object in self.interior.iter_objects() {
            // SAFETY: as in `select_all` — a stored object is always a validated
            // tuple, so its element slice is readable for the borrow's duration.
            let elements = unsafe { object_elements(object) };
            for clause in &spec.clauses {
                // `compile` only ever yields empty guards (M1 subset).
                if !matches!(clause.guard, MatchGuard::Empty) {
                    continue;
                }
                let mut binds = Vec::new();
                if match_head(&clause.head, elements, atoms, &mut binds) {
                    selected.push(resolve_body(&clause.body, object, &binds)?);
                    break;
                }
            }
        }
        Ok(selected)
    }
}

/// Match one object (via its tuple element slice) against a clause head,
/// collecting the head binds. Returns whether the head matched.
fn match_head(
    pattern: &MatchPattern,
    elements: &[Eterm],
    atoms: &AtomTable,
    binds: &mut Vec<(u32, Eterm)>,
) -> bool {
    match pattern {
        // A bare `'_'` head matches any object.
        MatchPattern::Wildcard => true,
        MatchPattern::Tuple(patterns) => {
            if patterns.len() != elements.len() {
                return false;
            }
            patterns
                .iter()
                .zip(elements)
                .all(|(pattern, element)| match pattern {
                    MatchElement::Wildcard => true,
                    MatchElement::Literal(literal) => eq(*literal, *element, atoms),
                    MatchElement::Bind(index) => {
                        binds.push((*index, *element));
                        true
                    }
                })
        }
    }
}

/// Build a matched clause's result from its body template and the head binds.
///
/// M1 can only produce a *single-element* result: constructing a fresh tuple
/// needs a target heap this API does not have (see the crate `CONTEXT.md`,
/// Known Issues), so a zero- or multi-element tuple body is `badarg`.
fn resolve_body(body: &MatchBody, object: Eterm, binds: &[(u32, Eterm)]) -> Result<Eterm> {
    match body {
        MatchBody::WholeObject => Ok(object),
        MatchBody::Tuple(elements) => {
            if elements.len() != 1 {
                return Err(badarg());
            }
            match &elements[0] {
                MatchBodyElement::Literal(literal) => Ok(*literal),
                MatchBodyElement::WholeObject => Ok(object),
                MatchBodyElement::Bind(index) => binds
                    .iter()
                    .find(|(var, _)| var == index)
                    .map(|(_, value)| *value)
                    .ok_or_else(badarg),
            }
        }
    }
}
