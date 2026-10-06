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
use crate::kind::{can_read, can_write, Access, TableKind, TableOptions};
use crate::ordered_interior::OrderedSetInterior;
use crate::select::SelectPredicate;
use crate::store::TermStore;

mod access;
mod update;

pub use access::TableAccess;
pub(crate) use update::{add_counters, counter_slot, replace_elements};
use update::{build_list, decode_counter_ops, object_arity, read_counter, warp_fires};

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

    // ------------------------------------- removal, navigation and storage

    /// Remove `key`'s entry and return **every** object stored under it (an empty
    /// vector when `key` is absent), mirroring `ets:take/2`.
    ///
    /// The drop-in default locates the objects and then deletes the entry; the
    /// hash interior overrides it with its one-pass removal.
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
        let newcnt = add_counters(read_counter(object, pos)?, incr)?;
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
            memory: self.memory(),
            owner: self.owner,
            named_table: self.opts.is_named(),
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
        // A single-object kind (`set`/`ordered_set`) *replaces* the object stored
        // under an existing key. Look the replaced root up **before** the insert so
        // its storage can be reclaimed afterwards (the multi kinds only ever add,
        // so there is nothing to reclaim).
        let replaced = if self.opts.kind().is_multi() {
            Vec::new()
        } else {
            self.interior.lookup(stored_key, atoms)
        };
        self.interior.insert(stored_key, stored, atoms)?;
        for old in replaced {
            self.store.free(old);
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

    /// The `pos`-th element (1-based) of the single object stored under `key`;
    /// `badarg` when `key` is absent or `pos` is out of range.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        // The public position is 1-based; `0` is `badarg` (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        self.interior.lookup_element(key, pos, atoms)
    }

    /// `ets:lookup_element/4`: exactly [`lookup_element`](Table::lookup_element),
    /// except that an absent `key` yields `default` instead of `badarg`.
    ///
    /// Verified against the system `erl` (OTP 29 / erts-17.1): `pos` is 1-based and
    /// `pos == 0` is `badarg` whatever the key; an **absent** key returns `default`
    /// even when `pos` is beyond the object's arity; a **present** key with a `pos`
    /// outside the object's arity is `badarg`.
    pub fn lookup_element_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Eterm> {
        // `pos` is validated before the key is consulted (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        if !self.interior.member(key, atoms) {
            return Ok(default);
        }
        self.interior.lookup_element(key, pos, atoms)
    }

    /// The number of words allocated to the table (`ets:info(Tab, memory)`): the
    /// table-owned term storage plus the interior's own index storage.
    pub fn memory(&self) -> usize {
        self.store.words() + self.interior.memory()
    }

    // ----------------------------------------------------------- element updates

    /// `ets:update_counter/3`: add `incr` to the element following the key
    /// (`keypos + 1`) of the object stored under `key`, and return the new value.
    ///
    /// Mirrors the verified system `erl` (OTP 29 / erts-17.1):
    /// * only `set`/`ordered_set` hold counters — `bag`/`duplicate_bag` are
    ///   `badarg`;
    /// * an **absent** `key` is `badarg`: the `/3` form has no default object;
    /// * a non-integer element or a non-integer increment is `badarg`.
    ///
    /// Despite the name, `update_op` may be any of the operands the BIF accepts
    /// (`Incr`, `{Pos,Incr}`, `{Pos,Incr,Threshold,SetValue}` or a list of those);
    /// a **list** operand yields a list result — exactly as `ets:update_counter/3`
    /// is `ets:update_counter/4` without a default.
    pub fn update_counter(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        self.do_update_counter(key, update_op, None, atoms, heap)
    }

    /// `ets:update_counter/4`: like [`update_counter`](Table::update_counter) but
    /// with a `default` object used when `key` is absent.
    ///
    /// Mirrors the verified system `erl` (OTP 29 / erts-17.1):
    /// * `default` must be a tuple with an arity of at least `keypos`, and an
    ///   invalid one is `badarg` even when `key` exists;
    /// * `update_op` is an increment, a `{Pos,Incr}`, a
    ///   `{Pos,Incr,Threshold,SetValue}`, or a list of those — only the list form
    ///   returns a **list** of the new values, one per operation, in order;
    /// * an absent `key` behaves as if the object `default` (with its key element
    ///   replaced by `key`) were present, and it is deposited in the table — even
    ///   when the operation list is empty;
    /// * `pos == keypos`, an out-of-range `pos`, a non-integer element and a
    ///   non-integer `Incr`/`Threshold`/`SetValue` are `badarg`;
    /// * the threshold rule: an increment `>= 0` whose result would be `>` the
    ///   threshold, or one `< 0` whose result would be `<` the threshold, makes
    ///   the counter `SetValue`.
    pub fn update_counter_with(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        default: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        self.do_update_counter(key, update_op, Some(default), atoms, heap)
    }

    /// `ets:update_element/3`: replace element `pos` (1-based) of the object
    /// stored under `key` with each `Value` of `updates` (later entries win for a
    /// repeated position), and return whether `key` was present.
    ///
    /// Mirrors the verified system `erl` (OTP 29 / erts-17.1):
    /// * an **absent** `key` is `Ok(false)`, never an error — and it is answered
    ///   **before** the spec is inspected, so a malformed spec on an absent key is
    ///   still `Ok(false)`;
    /// * `bag`/`duplicate_bag` are `badarg`;
    /// * `pos == keypos`, `pos == 0` and `pos` beyond the object's arity are
    ///   `badarg`, with nothing written (every position is validated first);
    /// * an **empty** `updates` on a present key is `Ok(true)` and changes
    ///   nothing.
    ///
    /// The `pos == keypos` rule is enforced here (the interiors do not carry
    /// `keypos`); the interior rebuilds the object.
    pub fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<bool> {
        self.check_counter_kind()?;
        let keypos = self.opts.keypos();
        // The key is looked up *before* the spec is validated: `erl` answers
        // `false` for an absent key whatever the spec says.
        let replaced = match self.interior.lookup(key, atoms).into_iter().next() {
            Some(object) => object,
            None => return Ok(false),
        };
        let arity = object_arity(replaced)?;
        for &(pos, _) in updates {
            if pos == keypos || pos == 0 || pos > arity {
                return Err(badarg());
            }
        }
        let updated = self.interior.update_element(key, updates, atoms, heap)?;
        if updated {
            // The interior rebuilt the object on the caller's `heap`; make it
            // table-owned (the A.1 copy boundary).
            self.relocate_rebuilt_object(key, replaced, atoms)?;
        }
        Ok(updated)
    }

    /// The shared `set`/`ordered_set` guard of the `ets:update_*` element
    /// operations: `bag`/`duplicate_bag` have no counters (`erl` raises `badarg`).
    fn check_counter_kind(&self) -> Result<()> {
        match self.opts.kind() {
            TableKind::Set | TableKind::OrderedSet => Ok(()),
            TableKind::Bag | TableKind::DuplicateBag => Err(badarg()),
        }
    }

    /// The common body of `ets:update_counter/3` (`default = None`) and `/4`
    /// (`default = Some`).
    ///
    /// The `default` object, when given, is validated before the key is consulted
    /// — `erl` fails on an invalid default even when the key exists. Every
    /// operation is validated before the first write, so a bad operation leaves
    /// the table (including a not-yet-deposited default object) untouched.
    ///
    /// Verified against the system `erl` (OTP 29 / erts-17.1):
    /// * `update_op` is `Incr | {Pos,Incr} | {Pos,Incr,Threshold,SetValue} |` a
    ///   list of those; only the **list** operand yields a list result;
    /// * with `default`, an absent key behaves as if the object `default` (with
    ///   its key element replaced by `key`) were present — it is deposited in the
    ///   table, even when the operation list is empty;
    /// * the threshold rule: an increment `>= 0` whose result would be `>` the
    ///   threshold, or one `< 0` whose result would be `<` the threshold, makes
    ///   the counter `SetValue`.
    fn do_update_counter(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        default: Option<Eterm>,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        self.check_counter_kind()?;
        let keypos = self.opts.keypos();
        // Validate the default object before consulting the key.
        let default = match default {
            Some(default) => {
                let arity = object_arity(default)?;
                if arity < keypos {
                    return Err(badarg());
                }
                Some((default, arity))
            }
            None => None,
        };
        let (ops, list_result) = decode_counter_ops(update_op, keypos)?;

        // The object to update: the stored one, or (the `/4` form only) the
        // default with its key element replaced by `key`, built on `heap`.
        let stored = self.interior.lookup(key, atoms).into_iter().next();
        let (object, deposit) = match stored {
            Some(object) => (object, false),
            None => match default {
                Some((default, arity)) => (
                    replace_elements(default, arity, &[(keypos, key)], heap)?,
                    true,
                ),
                // `/3` has no default object: an absent key is `badarg`.
                None => return Err(badarg()),
            },
        };

        // Validate every operation before the first write so the call is
        // all-or-nothing on a bad operand.
        let arity = object_arity(object)?;
        for op in &ops {
            if op.pos == keypos || op.pos == 0 || op.pos > arity {
                return Err(badarg());
            }
            // The addressed element must be an integer (and small — the bignum
            // element is the documented representation gap).
            read_counter(object, op.pos)?;
        }

        // Past this point the operations are applied; deposit the default object
        // first when the key was absent.
        let mut replaced = object;
        if deposit {
            self.insert(object, atoms)?;
            replaced = self
                .interior
                .lookup(key, atoms)
                .into_iter()
                .next()
                .ok_or_else(badarg)?;
        }

        let mut results = Vec::with_capacity(ops.len());
        for op in &ops {
            let value = self
                .interior
                .update_counter(key, op.pos, op.incr, atoms, heap)?;
            let value = match op.warp {
                Some((threshold, set_value)) if warp_fires(op.incr, value, threshold) => {
                    // The threshold rule replaces the counter outright; the hash
                    // override has no "write this value" entry point, so route it
                    // through `update_element` (same observable result).
                    self.interior
                        .update_element(key, &[(op.pos, set_value)], atoms, heap)?;
                    set_value
                }
                _ => value,
            };
            results.push(value);
        }
        // The last interior operation may have rebuilt the object on the caller's
        // `heap`; reconcile the index with table-owned storage once.
        self.relocate_rebuilt_object(key, replaced, atoms)?;

        if list_result {
            build_list(heap, &results)
        } else {
            // The scalar operands always decode to exactly one operation.
            results.first().copied().ok_or_else(badarg)
        }
    }

    /// Reconcile table-owned storage after an interior element update rebuilt the
    /// object stored under `key` on the caller's `heap`.
    ///
    /// The A.1 copy boundary forbids the table from retaining a pointer into a
    /// caller heap, so a rebuilt object is deep-copied into the table store, the
    /// store copy replaces it in the index, and the table-owned object it replaced
    /// is reclaimed. An **in-place** update (the hash index's `update_counter`)
    /// leaves the stored root untouched, and then there is nothing to do.
    fn relocate_rebuilt_object(
        &mut self,
        key: Eterm,
        replaced: Eterm,
        atoms: &AtomTable,
    ) -> Result<()> {
        let current = match self.interior.lookup(key, atoms).into_iter().next() {
            Some(current) => current,
            // The `set`/`ordered_set` kinds this path serves always keep exactly
            // one object per key.
            None => return Err(badarg()),
        };
        if current == replaced {
            return Ok(());
        }
        match self.store.copy(current) {
            Ok(stored) => {
                self.interior.insert(key, stored, atoms)?;
                self.store.free(replaced);
                Ok(())
            }
            Err(err) => {
                // The rebuilt object is not expressible in table storage (a fun,
                // an external pid, …): put the table-owned object back rather than
                // retain a caller-heap pointer, and report the failure.
                self.interior.insert(key, replaced, atoms)?;
                Err(err)
            }
        }
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
    /// The returned objects **alias table storage** and stay valid for the table's
    /// lifetime: this call deliberately does **not** reclaim them
    /// ([`TermStore::free`](crate::store::TermStore::free) would return their runs
    /// to the store's free lists, where a later `copy` could hand the same words
    /// out again). Their storage is released by
    /// [`delete_all_objects`](Table::delete_all_objects)/table drop.
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        self.interior.take(key, atoms)
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
