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

use crate::error::{badarg, unsupported};
use crate::hash_interior::HashInterior;
use crate::info::TableInfo;
use crate::key::{key_of, object_elements};
use crate::kind::{can_read, can_write, Access, TableKind, TableOptions};
use crate::ordered_interior::OrderedSetInterior;
use crate::select::SelectPredicate;
use crate::store::TermStore;

mod access;
mod element;
mod ownership;
mod update;

pub use access::TableAccess;
pub use ownership::{Heir, SetOpt};
pub(crate) use update::{add_counters, counter_slot, replace_elements};
use update::{object_arity, read_counter};

/// The process-global source of table identifiers (`ets:new/2` hands one out per
/// table). Monotonic and never reused; the identifier itself is opaque to
/// callers.
static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

/// The `what` of the typed "unsupported" raise the **scalar**
/// `ets:lookup_element/3,4` produces for a multi-object kind: the oracle's answer
/// there is the element *list*, never a single element, so no scalar can be
/// returned for it (see [`Table::lookup_element_all`]).
const MULTI_ELEMENT_SCALAR: &str =
    "ets:lookup_element/3,4 on bag/duplicate_bag (the result is a list; \
     use lookup_element_all/3)";
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
    /// (replace for `set`/`ordered_set`), and report whether the object was
    /// **retained**.
    ///
    /// `true` means the interior stored `object` (or replaced the entry with it)
    /// and now owns it; `false` means the interior rejected the object as a
    /// duplicate no-op (`bag`), so it did **not** store it and the object is
    /// unreferenced. The caller ([`Table::insert`]) reclaims a rejected copy from
    /// the store, so a rejected duplicate never leaks storage. `set`/`ordered_set`
    /// always retain (a replace keeps the *new* object) and `duplicate_bag` always
    /// retains; only `bag` can answer `false`.
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool>;

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

    /// The `pos`-th element (0-based) of **every** object stored under `key`, in
    /// this interior's stored order — `ets:lookup_element/3`'s result for a
    /// multi-object kind (`bag`/`duplicate_bag`), where the oracle returns a
    /// *list*.
    ///
    /// `badarg` when `key` is absent, and when **any** stored object's arity is at
    /// most `pos` — the oracle fails the whole call, it does not skip the short
    /// object (verified against the system `erl`, OTP 29 / erts-17.1). The
    /// single-object kinds (`set`/`ordered_set`) store at most one object per key,
    /// so their list has exactly one element.
    ///
    /// The default reads the objects through [`lookup`](Interior::lookup) and
    /// projects each with [`key_of`] (which is what rejects a short object); an
    /// interior may override it with a single-pass version.
    fn lookup_elements(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        let objects = self.lookup(key, atoms);
        if objects.is_empty() {
            return Err(badarg());
        }
        let mut elements = Vec::with_capacity(objects.len());
        for object in objects {
            elements.push(key_of(object, keypos)?);
        }
        Ok(elements)
    }
    /// The total number of stored objects (not keys, for the multi kinds).
    fn size(&self) -> usize;

    /// Every stored object, ordered for `ordered_set`.
    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_>;

    // ------------------------------------- removal, navigation and storage

    /// Remove `key`'s entry and return **every** object stored under it (an empty
    /// vector when `key` is absent), mirroring `ets:take/2`.
    ///
    /// The returned objects **alias table storage** (they are the interior's own
    /// copies); [`Table::take`], the only caller through the public API, deep-copies
    /// them out of the store and then reclaims these originals, so no returned pointer
    /// is ever a reclaimed word. The drop-in default locates the objects and then
    /// deletes the entry; the hash interior overrides it with its one-pass removal.
    fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let objects = self.lookup(key, atoms);
        if !objects.is_empty() {
            self.delete(key, atoms)?;
        }
        Ok(objects)
    }

    /// Every stored key, in this interior's own iteration order (ascending
    /// `beam_term` order for `ordered_set`, bucket-scan order for the hash kinds).
    fn keys(&self) -> Vec<Eterm>;

    /// The first key in this interior's iteration order (`ets:first/1`); `None`
    /// when the table is empty (`'$end_of_table'`).
    fn first_key(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.keys().first().copied()
    }

    /// The last key in this interior's iteration order (`ets:last/1`); `None`
    /// when the table is empty.
    fn last_key(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.keys().last().copied()
    }

    /// The key following `key` in this interior's iteration order, or `None` at
    /// the end of that order.
    ///
    /// An **absent** `key` has no position in the iteration order, so the answer
    /// falls back to the nearest key in `beam_term` term order (the first key
    /// strictly greater than `key`); that is exactly the `ordered_set` range
    /// semantics, and it gives the hash default a well-defined answer too.
    fn next_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let keys = self.keys();
        match keys.iter().position(|&candidate| eq(candidate, key, atoms)) {
            Some(position) => keys.get(position + 1).copied(),
            None => keys
                .into_iter()
                .find(|&candidate| cmp(candidate, key, atoms).is_gt()),
        }
    }

    /// The key preceding `key` in this interior's iteration order, or `None` at
    /// the start of that order (the mirror of [`Interior::next_key`]).
    fn prev_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let keys = self.keys();
        match keys.iter().position(|&candidate| eq(candidate, key, atoms)) {
            Some(position) => position.checked_sub(1).map(|before| keys[before]),
            None => keys
                .into_iter()
                .rev()
                .find(|&candidate| cmp(candidate, key, atoms).is_lt()),
        }
    }

    /// The number of `Eterm`-sized words this interior has allocated for its own
    /// index (`ets:info(Tab, memory)`'s interior contribution).
    ///
    /// The default is `0` — an interior that does not account for its index
    /// storage contributes nothing; the hash and `ordered_set` interiors report
    /// their honest bucket/entry and B+-tree node-arena allocations respectively.
    fn memory(&self) -> usize {
        0
    }

    // ------------------------------------- element updates (`ets:update_*`)

    /// `ets:update_counter/3,4` on a single position: add `incr` to element `pos`
    /// (1-based) of the object stored under `key` and return the **new** counter
    /// value.
    ///
    /// The drop-in default reads the element, adds through the shared
    /// [`add_counters`] helper, rebuilds the object on `heap` and inserts it back
    /// — which is exactly what the B+-tree interior (no writable index slot)
    /// needs. The hash interior overrides it with a destructive element write.
    ///
    /// `badarg` when `key` is absent, `pos` is outside `1..=arity`, or the element
    /// / `incr` is not an integer. The `pos == keypos` rule is the caller's: the
    /// interior does not know `keypos`.
    fn update_counter(
        &mut self,
        key: Eterm,
        pos: usize,
        incr: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        let object = self.lookup(key, atoms).into_iter().next().ok_or_else(badarg)?;
        let arity = object_arity(object)?;
        let newcnt = add_counters(read_counter(object, pos)?, incr, heap)?;
        let updated = replace_elements(object, arity, &[(pos, newcnt)], heap)?;
        self.insert(key, updated, atoms)?;
        Ok(newcnt)
    }

    /// `ets:update_element/3`: replace element `pos` (1-based) of the object
    /// stored under `key` with each `[pos, value]` of `updates` (later entries win
    /// for a repeated position), building the replacement on `heap`. Returns
    /// whether `key` existed.
    ///
    /// The drop-in default rebuilds the object on `heap` and inserts it back,
    /// which is what the B+-tree interior needs; the hash interior overrides it.
    ///
    /// `Ok(false)` for an absent `key` (not an error), `badarg` for a `pos`
    /// outside `1..=arity` — with **nothing** written, since every position is
    /// validated before any is applied.
    fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<bool> {
        let object = match self.lookup(key, atoms).into_iter().next() {
            Some(object) => object,
            None => return Ok(false),
        };
        let arity = object_arity(object)?;
        for &(pos, _) in updates {
            if pos == 0 || pos > arity {
                return Err(badarg());
            }
        }
        if updates.is_empty() {
            // Nothing to change; the key existed, so `ets` reports `true`.
            return Ok(true);
        }
        let updated = replace_elements(object, arity, updates, heap)?;
        self.insert(key, updated, atoms)?;
        Ok(true)
    }
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
    /// The heir installed with `ets:setopts/2` (`{heir, Pid, Data}`), if any.
    heir: Option<Heir>,
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
            heir: None,
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

    /// The registered table name, if any.
    ///
    /// Independent of `named_table`: a name is recorded for every table the
    /// `ets:new/2` option parser named, whether or not `named_table` was given.
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

    /// `ets:give_away/3`. Transfers ownership from `caller` to `new_owner`.
    ///
    /// `badarg` (owner unchanged) when `caller` is not the current owner, when
    /// `new_owner` is `None` (no live target process — the crate's stand-in for the
    /// oracle's non-pid/dead-pid target), or when `new_owner` is the current owner
    /// (the caller's own pid). On success the owner becomes `new_owner`; the
    /// `{'ETS-TRANSFER', …}` message to the new owner is the BIF/runtime layer's
    /// job, not this crate's (see [`super::ownership`]).
    pub fn give_away(
        &mut self,
        caller: Option<OwnerId>,
        new_owner: Option<OwnerId>,
    ) -> Result<()> {
        // Mirror the C `ets_give_away_3` order: bad-pid check, then the
        // not-owner check, then the self-gift check — all `badarg`.
        let Some(new_owner) = new_owner else {
            return Err(badarg());
        };
        if caller != self.owner {
            return Err(badarg());
        }
        if Some(new_owner) == self.owner {
            return Err(badarg());
        }
        self.owner = Some(new_owner);
        Ok(())
    }

    /// The table's current heir, if any.
    pub fn heir(&self) -> Option<Heir> {
        self.heir
    }

    /// Set (or clear) the heir directly (owner-level primitive; no access check).
    pub fn set_heir(&mut self, heir: Option<Heir>) {
        self.heir = heir;
    }

    /// `ets:setopts/2` for the `{heir, ...}` option: requires `caller` to be the
    /// current owner (`badarg` otherwise), then installs/clears the heir.
    pub fn setopts(&mut self, caller: Option<OwnerId>, opt: SetOpt) -> Result<()> {
        if caller != self.owner {
            return Err(badarg());
        }
        match opt {
            SetOpt::Heir(heir) => self.heir = Some(heir),
            SetOpt::ClearHeir => self.heir = None,
        }
        Ok(())
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
            memory: self.memory(),
            owner: self.owner,
            heir: self.heir.map(|h| h.owner),
            named_table: self.opts.is_named(),
            compressed: self.opts.is_compressed(),
            write_concurrency: self.opts.write_concurrency(),
            read_concurrency: self.opts.read_concurrency(),
        }
    }

    /// Insert `object` (the whole tuple; the key is derived from `keypos`).
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let stored = self.store_object(object)?;
        self.insert_stored(stored, atoms)
    }

    /// Validate `object` against the table's `keypos` and deep-copy it into
    /// table-owned storage (the A.1 copy boundary), returning the stored root.
    ///
    /// This is the whole fallible half of [`insert`](Table::insert): a non-tuple /
    /// too-short object is `badarg` **before** anything is allocated, and an object
    /// outside the store's supported subset (a fun, an external pid, …) rolls its
    /// copy back. Neither the index nor the store is left changed on failure, which
    /// is what lets [`insert_list`](Table::insert_list) validate a whole list
    /// before mutating anything.
    fn store_object(&mut self, object: Eterm) -> Result<Eterm> {
        key_of(object, self.opts.keypos())?;
        self.store.copy(object)
    }

    /// Insert an object that is **already** table-owned ([`store_object`]'s
    /// result): derive the stored key, apply the kind's duplicate rules, and
    /// reclaim whatever the insert replaced or rejected.
    ///
    /// Infallible for this crate's interiors (they answer with the retained flag,
    /// never an error); it returns `Result` only because [`Interior::insert`] does.
    ///
    /// [`store_object`]: Table::store_object
    fn insert_stored(&mut self, stored: Eterm, atoms: &AtomTable) -> Result<()> {
        let stored_key = key_of(stored, self.opts.keypos())?;
        // A single-object kind (`set`/`ordered_set`) *replaces* the object stored
        // under an existing key. Look the replaced root up **before** the insert so
        // its storage can be reclaimed afterwards (the multi kinds only ever add,
        // so there is nothing to reclaim).
        let replaced = if self.opts.kind().is_multi() {
            Vec::new()
        } else {
            self.interior.lookup(stored_key, atoms)
        };
        let retained = self.interior.insert(stored_key, stored, atoms)?;
        if !retained {
            // The interior rejected the copy as a duplicate no-op (a `bag` holding
            // an identical object): no entry references it, so reclaim it instead of
            // leaving it orphaned in the store.
            self.store.free(stored);
        }
        for old in replaced {
            self.store.free(old);
        }
        Ok(())
    }

    /// Insert every object of `objects` (`ets:insert/2` over a list).
    ///
    /// **Atomic**, exactly like the oracle: `ets:insert(Tab, List)` validates the
    /// whole list first, so if **any** element is not a valid object for the
    /// table's `keypos` (a non-tuple, or a tuple shorter than `keypos`) the call is
    /// `badarg` and **none** of the elements is inserted — not even the valid ones
    /// that precede the bad one. Verified against the system `erl` (OTP 29 /
    /// erts-17.1): with `{1,10}` in `T`, `ets:insert(T, [{2,20}, foo])` raises
    /// `badarg` and `ets:tab2list(T)` is still `[{1,10}]`. An empty list inserts
    /// nothing and succeeds.
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()> {
        // Validate and copy every object **before** the first index mutation, so a
        // bad element cannot leave a half-applied list behind.
        let mut stored = Vec::with_capacity(objects.len());
        for &object in objects {
            match self.store_object(object) {
                Ok(one) => stored.push(one),
                Err(err) => {
                    // Nothing has been inserted: release the copies made so far and
                    // report, leaving the table exactly as it was.
                    for one in stored {
                        self.store.free(one);
                    }
                    return Err(err);
                }
            }
        }
        // Every object is valid and table-owned; insert them in list order.
        for one in stored {
            self.insert_stored(one, atoms)?;
        }
        Ok(())
    }
    /// Insert `object` only if its key is absent; returns whether it was
    /// inserted (`false` when the key already exists).
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let keypos = self.opts.keypos();
        key_of(object, keypos)?;
        let stored = self.store.copy(object)?;
        let stored_key = key_of(stored, keypos)?;
        let inserted = self.interior.insert_new(stored_key, stored, atoms)?;
        if !inserted {
            // The key already exists, so the interior stored nothing: the copy is
            // unreferenced and is reclaimed.
            self.store.free(stored);
        }
        Ok(inserted)
    }

    /// Delete every object stored under `key` (`ets:delete/2`).
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        // Capture the objects about to be removed so their table-owned storage is
        // reclaimed (a `set`/`ordered_set` entry holds one object, a `bag`/
        // `duplicate_bag` entry may hold many).
        let removed = self.interior.lookup(key, atoms);
        self.interior.delete(key, atoms)?;
        for object in removed {
            self.store.free(object);
        }
        Ok(())
    }

    /// Delete the single object matching `object` (`ets:delete_object/2`).
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        // The key is only used to locate the entry; the object itself is compared.
        let key = key_of(object, self.opts.keypos())?;
        // Capture exactly the objects the interior will remove — the stored objects
        // under `key` that equal `object` — so their storage is reclaimed afterwards.
        let removed: Vec<Eterm> = self
            .interior
            .lookup(key, atoms)
            .into_iter()
            .filter(|&stored| eq(stored, object, atoms))
            .collect();
        self.interior.delete_object(key, object, atoms)?;
        for stored in removed {
            self.store.free(stored);
        }
        Ok(())
    }

    /// Delete every object in the table (`ets:delete_all_objects/1`).
    pub fn delete_all_objects(&mut self) {
        // `reset` releases every block the store owns (so the per-object runs of
        // the deleted objects go with them) and leaves the store immediately
        // usable again; a fresh interior of the same kind is then empty.
        self.store.reset();
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

    /// The `pos`-th element (1-based) of the object stored under `key`;
    /// `badarg` when `key` is absent or `pos` is out of range.
    ///
    /// Only `set`/`ordered_set` — which store at most one object per key — have a
    /// scalar answer. The multi kinds (`bag`/`duplicate_bag`) hold **several**
    /// objects under one key and `ets:lookup_element/3` returns a *list* of their
    /// `pos`-th elements; a scalar is therefore undefined for them, and this method
    /// raises the crate's typed
    /// [`unsupported`](crate::error::unsupported) instead of a wrong element. Use
    /// [`lookup_element_all`](Table::lookup_element_all) for those kinds.
    ///
    /// The oracle's *failures* are still answered exactly: `pos == 0`, an absent
    /// `key`, and a stored object whose arity is `< pos` are all `badarg`.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        // The public position is 1-based; `0` is `badarg` (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        if self.opts.kind().is_multi() {
            // `lookup_elements` reproduces the oracle's `badarg`s (absent key, a
            // short object); where the oracle has an answer it is the *list*, so a
            // scalar would be a wrong result.
            return self
                .interior
                .lookup_elements(key, pos, atoms)
                .and_then(|_| Err(unsupported(MULTI_ELEMENT_SCALAR)));
        }
        self.interior.lookup_element(key, pos, atoms)
    }

    /// `ets:lookup_element/4`: exactly [`lookup_element`](Table::lookup_element),
    /// except that an absent `key` yields `default` instead of `badarg`.
    ///
    /// Verified against the system `erl` (OTP 29 / erts-17.1): `pos` is 1-based and
    /// `pos == 0` is `badarg` whatever the key; an **absent** key returns `default`
    /// even when `pos` is beyond the object's arity; a **present** key with a `pos`
    /// outside the object's arity is `badarg`.
    ///
    /// Like the `/3` form it is scalar, so for `bag`/`duplicate_bag` it raises the
    /// typed [`unsupported`](crate::error::unsupported) for a present key (an absent
    /// key still yields `default`); use
    /// [`lookup_element_all_or`](Table::lookup_element_all_or) there.
    pub fn lookup_element_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Eterm> {
        // `pos` is validated before the key is consulted (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        let present = self.interior.member(key, atoms);
        match (self.opts.kind().is_multi(), present) {
            // An absent key is `default` whatever the kind.
            (_, false) => Ok(default),
            // A present key on a multi kind: the oracle's answer is the element
            // *list* (or `badarg` for an object too short to have `pos`).
            (true, true) => self
                .interior
                .lookup_elements(key, pos, atoms)
                .and_then(|_| Err(unsupported(MULTI_ELEMENT_SCALAR))),
            (false, true) => self.interior.lookup_element(key, pos, atoms),
        }
    }

    /// `ets:lookup_element/3` for a multi-object kind (`bag`/`duplicate_bag`): the
    /// `pos`-th element (1-based) of **every** object stored under `key`, in
    /// stored order.
    ///
    /// Heap-free, like [`lookup`](Table::lookup): the returned elements read
    /// table-owned storage (`Immediate` elements are copied by value), so no
    /// allocation or host heap is involved. Mirrors the system `erl` (OTP 29 /
    /// erts-17.1), where the multi kinds answer with a list: a key holding a single
    /// object yields a one-element list, duplicates (`duplicate_bag`) are
    /// preserved, an **absent** `key` is `badarg`, `pos == 0` is `badarg`, and one
    /// object whose arity is `< pos` makes the **whole** call `badarg`.
    ///
    /// `set`/`ordered_set` store at most one object per key, so for them the vector
    /// always holds exactly one element — the same element
    /// [`lookup_element`](Table::lookup_element) returns.
    pub fn lookup_element_all(
        &self,
        key: Eterm,
        pos: usize,
        atoms: &AtomTable,
    ) -> Result<Vec<Eterm>> {
        // The public position is 1-based; `0` is `badarg` (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        self.interior.lookup_elements(key, pos, atoms)
    }

    /// `ets:lookup_element/4` for a multi-object kind: exactly
    /// [`lookup_element_all`](Table::lookup_element_all), except that an absent
    /// `key` is reported as `Ok(None)` — the caller yields `default` — instead of
    /// `badarg`.
    ///
    /// The split mirrors the oracle exactly: `pos == 0` is `badarg` **before** the
    /// key is consulted, an absent key yields `default` whatever `pos`, and a
    /// present key keeps every failure of
    /// [`lookup_element_all`](Table::lookup_element_all). It is heap-free and
    /// cannot build `default` itself, which is why the absent key is a `None`
    /// rather than a term.
    pub fn lookup_element_all_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Option<Vec<Eterm>>> {
        // `pos` is validated before the key is consulted (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        // `default` belongs to the caller: this method reports the absent key as
        // `None` and the BIF layer substitutes the term.
        let _ = default;
        if !self.interior.member(key, atoms) {
            return Ok(None);
        }
        Ok(Some(self.interior.lookup_elements(key, pos, atoms)?))
    }
    /// The number of words allocated to the table (`ets:info(Tab, memory)`): the
    /// table-owned term storage plus the interior's own index storage.
    pub fn memory(&self) -> usize {
        self.store.words() + self.interior.memory()
    }

    // --------------------------------------------------------------- navigation

    /// The first key in the table's iteration order (`ets:first/1`); `None` is the
    /// `'$end_of_table'` result and an empty table always yields it.
    pub fn first(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.interior.first_key(atoms)
    }

    /// The last key in the table's iteration order (`ets:last/1`); `None` is the
    /// `'$end_of_table'` result and an empty table always yields it.
    pub fn last(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.interior.last_key(atoms)
    }

    /// The key following `key` (`ets:next/2`); `Ok(None)` is the
    /// `'$end_of_table'` result.
    ///
    /// Mirrors the verified `erl` (OTP 29 / erts-17.1) behaviour: an `ordered_set`
    /// reads `key` as a **range bound** — an absent key is fine and the answer is
    /// the nearest stored key above it (so an empty table yields `'$end_of_table'`)
    /// — whereas the hash kinds (`set`/`bag`/`duplicate_bag`) walk an internal
    /// order anchored at a *present* entry and raise `badarg` for an unknown key,
    /// which includes every key of an empty table.
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.check_navigable(key, atoms)?;
        Ok(self.interior.next_key(key, atoms))
    }

    /// The key preceding `key` (`ets:prev/2`); `Ok(None)` is the
    /// `'$end_of_table'` result. Mirrors [`next`](Table::next) (verified against the
    /// system `erl`): a range bound for `ordered_set`, a present-key requirement
    /// (`badarg` otherwise) for the hash kinds.
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.check_navigable(key, atoms)?;
        Ok(self.interior.prev_key(key, atoms))
    }

    /// The guard shared by [`next`](Table::next)/[`prev`](Table::prev): only the
    /// hash kinds need the anchor key to be present.
    fn check_navigable(&self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        if self.opts.kind().is_ordered() || self.interior.member(key, atoms) {
            Ok(())
        } else {
            Err(badarg())
        }
    }

    /// Remove `key`'s entry and return every object stored under it
    /// (`ets:take/2`); an absent key yields an empty vector.
    ///
    /// The removed objects are **handed to the caller**: each is deep-copied out of
    /// table storage and the table's own original copy is reclaimed
    /// ([`TermStore::free`](crate::store::TermStore::free)), so a churn of `insert` +
    /// `take` does not grow the table's memory. The returned copies are
    /// [`relinquish`](crate::store::TermStore::relinquish)ed: they stay valid for the
    /// table's lifetime and their words are never reused (a later `copy` can never
    /// hand the caller's term out again), but they no longer count as table memory —
    /// exactly the real `ets:take/2`, where the objects move off the table.
    ///
    /// Every object is copied **before** any original is reclaimed: a copy reads its
    /// original's words, which `free` would return to the store's free lists.
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let removed = self.interior.take(key, atoms)?;
        if removed.is_empty() {
            return Ok(Vec::new());
        }
        // Deep-copy every removed object out first (see the doc above).
        let mut taken = Vec::with_capacity(removed.len());
        for &original in &removed {
            match self.store.copy(original) {
                Ok(copy) => taken.push(copy),
                Err(err) => {
                    // The entry is gone either way, so nothing may stay counted:
                    // reclaim the copies taken so far **and** every original before
                    // reporting. (In practice unreachable — a table-owned object is
                    // always a copy of a supported term — but the failure path must
                    // not leak.)
                    for copy in taken.drain(..) {
                        self.store.free(copy);
                    }
                    for &original in &removed {
                        self.store.free(original);
                    }
                    return Err(err);
                }
            }
        }
        // Hand the copies to the caller (no longer table memory, never reusable) and
        // reclaim the table's own originals.
        for &copy in &taken {
            self.store.relinquish(copy);
        }
        for &original in &removed {
            self.store.free(original);
        }
        Ok(taken)
    }

    /// Whether `requester` may **read** the table; an access violation is
    /// `BeamError::BadArg`, matching how `ets` reports it.
    pub fn check_read(&self, requester: Option<OwnerId>) -> Result<()> {
        if can_read(self.opts.access(), self.owner, requester) {
            Ok(())
        } else {
            Err(badarg())
        }
    }

    /// Whether `requester` may **write** the table; an access violation is
    /// `BeamError::BadArg`, matching how `ets` reports it.
    pub fn check_write(&self, requester: Option<OwnerId>) -> Result<()> {
        if can_write(self.opts.access(), self.owner, requester) {
            Ok(())
        } else {
            Err(badarg())
        }
    }

    /// An access-enforcing view of this table for the process `requester`.
    ///
    /// The [`Table`] methods are owner-level primitives — the operations the
    /// table's own runtime performs — and do not consult the access mode. A BIF
    /// acting for a specific process wraps the table in a [`TableAccess`], which
    /// rechecks `kind::can_read`/`can_write` before every operation and raises
    /// `BeamError::BadArg` on a violation, exactly like `ets`.
    pub fn as_requester(&mut self, requester: Option<OwnerId>) -> TableAccess<'_> {
        TableAccess {
            table: self,
            requester,
        }
    }

    /// The number of objects in the table.
    ///
    /// The `ets` accessor is `ets:info(T, size)`; `ets:size/1` does not exist in
    /// this OTP (OTP 29 / erts-17.1), so the differential battery reads it through
    /// `ets:info/2`.
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

}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_term::heap::Heap;

    /// A `{Key, Value}` object (keypos 1) on `heap`.
    fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    /// A `{Key, Value, Tag}` object (keypos 1) on `heap`.
    fn triple(heap: &mut Heap, key: i64, value: i64, tag: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value), make_small(tag)])
            .unwrap()
    }

    /// A fresh keypos-1 table of `kind`.
    fn table(kind: TableKind) -> Table {
        let mut opts = TableOptions::new();
        opts.set_kind(kind);
        Table::new(TableId::next(), opts).expect("a fresh table is constructible")
    }

    /// `err` is the crate's typed "unsupported operation" raise (not a `badarg`),
    /// and it names `needle`.
    fn assert_unsupported_naming(err: BeamError, needle: &str) {
        let raise = err.into_raise().expect("the unsupported marker is a raise");
        let reason = raise.reason.as_atom().expect("the reason is an atom");
        assert_eq!(raise.class, beam_common::ExceptionClass::Error);
        assert_eq!(
            reason,
            format!("unsupported_ets: {needle}"),
            "the raise names this crate's unsupported surface"
        );
    }
    /// The interior reports a rejected `bag` duplicate, so `Table::insert` reclaims
    /// the deep copy it just made: the store's live-word count does not grow.
    #[test]
    fn a_rejected_bag_duplicate_costs_no_store_words() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Bag);

        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        let live = t.store.words();
        assert!(live > 0, "the stored object owns store words");

        for _ in 0..10 {
            t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        }
        assert_eq!(
            t.store.words(),
            live,
            "a rejected duplicate's copy must be reclaimed, not orphaned"
        );
        assert_eq!(t.size(), 1);
    }

    /// `insert_new` reclaims the deep copy it made when the key already exists.
    #[test]
    fn a_refused_insert_new_costs_no_store_words() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Set);

        assert!(t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap());
        let live = t.store.words();

        for _ in 0..10 {
            assert!(!t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap());
        }
        assert_eq!(t.store.words(), live, "a refused insert_new leaks no copy");
        assert_eq!(t.size(), 1);
    }

    /// `take` reclaims the stored object (the store holds no live words afterwards)
    /// and hands the caller a checked-out copy that stays valid.
    #[test]
    fn take_reclaims_the_stored_object_and_returns_a_checked_out_copy() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Set);

        let source = object(&mut heap, 1, 10);
        t.insert(source, &atoms).unwrap();
        assert!(t.store.words() > 0);

        let taken = t.take(make_small(1), &atoms).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(
            t.store.words(),
            0,
            "the stored object is reclaimed; the returned copy is checked out"
        );
        assert_eq!(t.size(), 0);
        assert!(
            eq(taken[0], source, &atoms),
            "the returned copy is structurally the removed object"
        );
    }

    /// `insert_list` validates the whole list **before** the first store write, so
    /// an invalid element inserts none of the elements — including the valid ones
    /// that precede it (verified `erl` behaviour).
    #[test]
    fn insert_list_validates_every_object_before_inserting_any() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Set);

        let kept = object(&mut heap, 1, 10);
        t.insert(kept, &atoms).unwrap();
        let live = t.store.words();
        let good = object(&mut heap, 2, 20);

        // A non-tuple element after a valid one: nothing is inserted.
        assert!(matches!(
            t.insert_list(&[good, make_small(7)], &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.size(), 1, "only the pre-existing object survives");
        assert!(!t.member(make_small(2), &atoms));
        assert_eq!(
            t.store.words(),
            live,
            "the valid element's copy must not be left in the store"
        );

        // The invalid element may also come first.
        assert!(matches!(
            t.insert_list(&[make_small(7), good], &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.size(), 1);
        assert_eq!(t.store.words(), live);

        // A valid list still inserts everything (the failure above left no residue).
        t.insert_list(&[good, object(&mut heap, 3, 30)], &atoms).unwrap();
        assert_eq!(t.size(), 3);
    }

    /// A `{keypos,2}` table rejects a list containing an element whose arity is
    /// `< keypos` without inserting the valid elements either.
    #[test]
    fn insert_list_is_atomic_under_a_custom_keypos() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut opts = TableOptions::new();
        opts.set_keypos(2).unwrap();
        let mut t = Table::new(TableId::next(), opts).unwrap();

        let kept = heap.tuple(&[make_small(1), make_small(10)]).unwrap();
        t.insert(kept, &atoms).unwrap();
        let live = t.store.words();

        // `{b}` is a 1-tuple: arity 1 < keypos 2, so the whole list is `badarg`.
        let good = heap.tuple(&[make_small(2), make_small(20)]).unwrap();
        let short = heap.tuple(&[make_small(3)]).unwrap();
        assert!(matches!(
            t.insert_list(&[good, short], &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.size(), 1);
        assert!(t.member(make_small(10), &atoms));
        assert!(!t.member(make_small(20), &atoms));
        assert_eq!(t.store.words(), live);
    }

    /// The element projection of every object under a key, for all four kinds:
    /// the list `ets:lookup_element/3` returns for `bag`/`duplicate_bag`, and the
    /// one-element list the single-object kinds yield.
    #[test]
    fn lookup_element_all_projects_every_object_under_a_key() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);

        let mut bag = table(TableKind::Bag);
        bag.insert(triple(&mut heap, 1, 10, 100), &atoms).unwrap();
        bag.insert(triple(&mut heap, 1, 20, 200), &atoms).unwrap();
        let values = bag.lookup_element_all(make_small(1), 2, &atoms).unwrap();
        assert!(values
            .iter()
            .zip([make_small(10), make_small(20)])
            .all(|(got, want)| eq(*got, want, &atoms)));
        let tags = bag.lookup_element_all(make_small(1), 3, &atoms).unwrap();
        assert!(eq(tags[1], make_small(200), &atoms));

        // A key element repeats once per object.
        let keys = bag.lookup_element_all(make_small(1), 1, &atoms).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(eq(keys[0], make_small(1), &atoms) && eq(keys[1], make_small(1), &atoms));

        // `duplicate_bag` preserves identical objects (and so identical elements).
        let mut dup = table(TableKind::DuplicateBag);
        dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
        dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
        assert_eq!(dup.lookup_element_all(make_small(5), 2, &atoms).unwrap().len(), 2);

        // The single-object kinds store at most one object per key.
        for kind in [TableKind::Set, TableKind::OrderedSet] {
            let mut t = table(kind);
            t.insert(triple(&mut heap, 9, 10, 100), &atoms).unwrap();
            let one = t.lookup_element_all(make_small(9), 2, &atoms).unwrap();
            assert_eq!(one.len(), 1, "{kind:?} holds one object per key");
            assert!(eq(one[0], t.lookup_element(make_small(9), 2, &atoms).unwrap(), &atoms));
        }
    }

    /// The element-list projection's failures mirror the oracle: an absent key, a
    /// `pos` of 0, and an object too short to have `pos` are all `badarg`.
    #[test]
    fn lookup_element_all_rejects_absent_keys_zero_and_short_objects() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut bag = table(TableKind::Bag);
        bag.insert(object(&mut heap, 1, 10), &atoms).unwrap();

        assert!(matches!(
            bag.lookup_element_all(make_small(1), 0, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            bag.lookup_element_all(make_small(99), 2, &atoms),
            Err(BeamError::BadArg)
        ));
        // `{1,10}` has arity 2, so position 3 is out of range for the whole call.
        assert!(matches!(
            bag.lookup_element_all(make_small(1), 3, &atoms),
            Err(BeamError::BadArg)
        ));

        // `/4` reports the absent key as `None` (the caller yields `default`), but
        // validates `pos` first and keeps a present key's `badarg`.
        let default = make_small(-1);
        assert_eq!(
            bag.lookup_element_all_or(make_small(99), 9, default, &atoms).unwrap(),
            None
        );
        assert!(matches!(
            bag.lookup_element_all_or(make_small(99), 0, default, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            bag.lookup_element_all_or(make_small(1), 3, default, &atoms),
            Err(BeamError::BadArg)
        ));
        let present = bag
            .lookup_element_all_or(make_small(1), 2, default, &atoms)
            .unwrap()
            .expect("a present key yields the elements");
        assert_eq!(present.len(), 1);
        assert!(eq(present[0], make_small(10), &atoms));
    }

    /// The scalar `lookup_element`/`lookup_element_or` are honest for the
    /// multi-object kinds: they raise the crate's typed "unsupported" (never a
    /// wrong scalar) while still answering the oracle's failures with `badarg`.
    #[test]
    fn scalar_lookup_element_is_unsupported_for_multi_kinds() {
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(64);
            let mut t = table(kind);
            t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
            t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

            // A present key with an in-range position: the oracle returns a list.
            assert_unsupported_naming(
                t.lookup_element(make_small(1), 2, &atoms).unwrap_err(),
                MULTI_ELEMENT_SCALAR,
            );
            // ... and the `/4` form of the same call.
            assert_unsupported_naming(
                t.lookup_element_or(make_small(1), 2, make_small(-1), &atoms)
                    .unwrap_err(),
                MULTI_ELEMENT_SCALAR,
            );

            // The oracle's failures stay `badarg`, not "unsupported".
            assert!(matches!(
                t.lookup_element(make_small(1), 0, &atoms),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                t.lookup_element(make_small(99), 2, &atoms),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                t.lookup_element(make_small(1), 3, &atoms),
                Err(BeamError::BadArg)
            ));
            // An absent key is `default` for `/4`, whatever the kind.
            assert_eq!(
                t.lookup_element_or(make_small(99), 9, make_small(-1), &atoms)
                    .unwrap(),
                make_small(-1)
            );
            assert!(matches!(
                t.lookup_element_or(make_small(99), 0, make_small(-1), &atoms),
                Err(BeamError::BadArg)
            ));
        }
    }
}
