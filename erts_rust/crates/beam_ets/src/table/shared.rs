//! A thread-safe, shareable handle over a [`Table`]: [`SharedTable`].
//!
//! # Why this exists
//!
//! A bare [`Table`] is moveable and shareable (`Table: Send + Sync` — see the
//! `Interior` trait's `Send + Sync` supertraits and the `unsafe impl`s on
//! [`TermStore`](crate::store::TermStore)), but every *mutating* operation takes
//! `&mut self`, so two OS threads cannot both drive one table through the borrow
//! checker alone. [`SharedTable`] supplies the missing synchronisation: it wraps
//! the table in an `Arc<RwLock<Table>>`, so many threads may *read* concurrently
//! and a writer takes the table exclusively. It is the shared table of the Stage-3
//! plan (`../CONTEXT.md`, gap "ETS is single-threaded").
//!
//! # The lock is the whole story — no lost updates
//!
//! A writer holds the table for the duration of its `write` guard, and every
//! check-then-act sequence (`insert_new`, `update_counter`, `select_replace`, …)
//! runs *inside* that one guard, so no two writers interleave a read with a write.
//! The concurrency test in `tests/ets_shared.rs` proves it: `N` threads each insert
//! `M` distinct keys and the finished table reports exactly `N × M` objects.
//!
//! # Terms may not outlive the guard (the aliasing hazard)
//!
//! The A.1 copy boundary is on the **insert** side only: an inserted object is
//! deep-copied into the table-owned [`TermStore`](crate::store::TermStore) arena,
//! but a *read* hands back `Eterm`s that **alias** those table-owned words. The
//! arena never relocates, but it *reuses* words: a `delete` returns an object's
//! runs to the store's per-size free lists and a later insert of an object of the
//! same word size is handed exactly those words back. A term read under one guard
//! and used after that guard is dropped therefore silently re-reads as a
//! **different, well-formed object** — a wrong result, never a loud error.
//!
//! This handle does not hand the raw guard out. [`SharedTable::read`] and
//! [`SharedTable::write`] return the guard-scoped views
//! [`TableRead`]/[`TableWrite`], and *those* are the only source of terms, tagged
//! with the lifetime of the borrow of the view (see the `view` module). A term
//! that outlives its closure or its `read()` scope is a **compile error**:
//!
//! ```compile_fail
//! use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions};
//! use beam_term::prelude::*;
//!
//! let mut opts = TableOptions::new();
//! opts.set_kind(TableKind::Set);
//! opts.set_access(Access::Public);
//! let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
//! let atoms = AtomTable::new();
//!
//! // Does not compile: the terms alias table storage and borrow the closure-local
//! // view (which owns the read guard), so they cannot escape the closure.
//! let leaked = shared.with_read(|view| view.lookup(make_small(1), &atoms)).unwrap();
//! let _ = leaked.len();
//! ```
//!
//! A caller that needs an *owned* term says so explicitly, with the allocating
//! copy-out [`TableTerm::copy_into`](crate::table::TableTerm::copy_into).
//!
//! # Poisoning is reported, never swallowed
//!
//! A thread that panics while holding the lock poisons it. Because a panic mid
//! mutation may have left the table's invariants broken, [`SharedTable::read`] and
//! [`SharedTable::write`] refuse to hand the table out and raise the crate's typed
//! `unsupported_ets: table lock poisoned` instead — an honest failure, never a
//! silently-wrong result (workspace `CONTEXT.md` §3).
//!
//! # What is *not* here
//!
//! This is coarse-grained, exclusive-writer locking — the semantics of a
//! `private`/`protected`/`public` table guarded by one lock, not the fine-grained
//! per-bucket concurrency of `{write_concurrency, true}` (whose *observable* effects
//! are only throughput, not results). A later pass may shard the lock; the
//! observable `ets:*` semantics this crate owns are unaffected either way.
//!
//! The views are `!Send`: a [`TableRead`] carries a lazily-allocated scratch
//! [`Heap`](beam_alloc::Heap) for the copy-out select family, and a bare
//! `beam_alloc::Heap` is deliberately not `Send`. Moving a view — and with it terms
//! aliasing the table — to another thread is exactly what must not happen, so this
//! is a feature, not a limitation.

use std::sync::{Arc, RwLock};

use beam_common::Result;

use super::{Table, TableId, TableRead, TableWrite};

/// The typed raise a poisoned table lock produces (never a silent `None`).
const POISONED: &str = "table lock poisoned";

/// A reference-counted, lock-protected handle to an ETS [`Table`].
///
/// Clone it to hand the *same* table to another thread; every clone shares one
/// `Arc<RwLock<Table>>`. Construct one with [`SharedTable::new`] and reach the
/// table through [`read`](SharedTable::read) / [`write`](SharedTable::write), or
/// the closure forms [`with_read`](SharedTable::with_read) /
/// [`with_write`](SharedTable::with_write).
pub struct SharedTable {
    /// The table plus the read/write lock guarding it. `Arc` makes the handle
    /// cloneable; `RwLock` makes concurrent reads possible and writers exclusive.
    inner: Arc<RwLock<Table>>,
}

impl SharedTable {
    /// Wrap `table` in a fresh lock, taking ownership.
    pub fn new(table: Table) -> SharedTable {
        SharedTable {
            inner: Arc::new(RwLock::new(table)),
        }
    }

    /// The table identifier (stable, so it needs no lock).
    pub fn id(&self) -> TableId {
        // `RwLock::read` cannot fail to *read* the id here without poisoning; the
        // id never changes after construction, so borrow it through whatever view
        // is available. A poisoned lock is reported rather than panicking.
        match self.inner.read() {
            Ok(table) => table.id(),
            // A poisoned lock still yields the guard's contents; the id is not part
            // of the mutable state a panicking writer could have half-written, so
            // recovering it here is sound and cannot report a wrong id. (`read`
            // would otherwise raise, but `id` is an infallible accessor.)
            Err(poisoned) => poisoned.into_inner().id(),
        }
    }

    /// How many handles share this table (including this one).
    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// Take the table for reading (many readers may hold this at once).
    ///
    /// Returns a [`TableRead`] rather than the raw `RwLockReadGuard`: the view owns
    /// the guard, and the terms it hands out are lifetime-tagged to the view's
    /// borrow, so a read result cannot outlive the guard (see the [module
    /// docs](self) and the `view` module).
    ///
    /// Reports the typed `unsupported_ets: table lock poisoned` raise when a writer
    /// panicked, rather than handing out a table whose invariants may be broken.
    pub fn read(&self) -> Result<TableRead<'_>> {
        Ok(TableRead::new(
            self.inner
                .read()
                .map_err(|_| crate::error::unsupported(POISONED))?,
        ))
    }

    /// Take the table for writing (exclusive).
    ///
    /// Returns a [`TableWrite`] (see [`read`](SharedTable::read)).
    ///
    /// Reports the typed `unsupported_ets: table lock poisoned` raise when a writer
    /// panicked.
    pub fn write(&self) -> Result<TableWrite<'_>> {
        Ok(TableWrite::new(
            self.inner
                .write()
                .map_err(|_| crate::error::unsupported(POISONED))?,
        ))
    }

    /// Run `f` against the table under a read lock and return its result.
    ///
    /// The closure receives `&mut TableRead<'_>`, so it can extract owned data
    /// (`size`, `info`, …) or copy terms out with
    /// [`copy_into`](crate::table::TableTerm::copy_into) — but it **cannot** return
    /// a term that aliases table storage, because the term's lifetime is tied to the
    /// closure-local view and `T` cannot name that lifetime. That is the whole point
    /// (see the [module docs](self)).
    pub fn with_read<T>(&self, f: impl FnOnce(&mut TableRead<'_>) -> T) -> Result<T> {
        let mut view = self.read()?;
        Ok(f(&mut view))
    }

    /// Run `f` against the table under an exclusive write lock and return its
    /// result. The whole closure runs inside the one guard, so a check-then-act
    /// sequence is atomic with respect to other threads.
    pub fn with_write<T>(&self, f: impl FnOnce(&mut TableWrite<'_>) -> T) -> Result<T> {
        let mut view = self.write()?;
        Ok(f(&mut view))
    }

    /// Consume this handle and return the table, when it is the **only** handle.
    ///
    /// `Ok(None)` when other [`SharedTable`] clones still exist (the table cannot
    /// be uniquely owned); the caller may then retry after dropping them.
    pub fn try_into_inner(self) -> Result<Option<Table>> {
        match Arc::try_unwrap(self.inner) {
            Ok(lock) => match lock.into_inner() {
                Ok(table) => Ok(Some(table)),
                // Poisoned by a panicking writer, but the caller now owns the table
                // exclusively and asked for it explicitly: hand it over rather than
                // dropping a table the caller may still want.
                Err(poisoned) => Ok(Some(poisoned.into_inner())),
            },
            Err(_) => Ok(None),
        }
    }
}

impl Clone for SharedTable {
    /// A clone shares the same table (the same `Arc`); it never copies it.
    fn clone(&self) -> SharedTable {
        SharedTable {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl core::fmt::Debug for SharedTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedTable")
            .field("id", &self.id())
            .field("handles", &self.ref_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::{Access, TableOptions};
    use crate::table::OwnerId;

    /// A fresh still-open table for the unit tests.
    fn table() -> Table {
        let mut opts = TableOptions::new();
        opts.set_access(Access::Public);
        Table::new(TableId::next(), opts).expect("a fresh table is constructible")
    }

    #[test]
    fn clones_share_one_table_and_one_id() {
        let shared = SharedTable::new(table());
        let other = shared.clone();
        assert_eq!(shared.id(), other.id());
        assert_eq!(shared.ref_count(), 2);
        // A mutation through one handle is visible through the other: they really
        // are the same table, not two copies.
        let owner = OwnerId::new(7);
        other.with_write(|t| t.set_owner(Some(owner))).unwrap();
        assert_eq!(shared.with_read(|t| t.owner()).unwrap(), Some(owner));
        assert_eq!(shared.ref_count(), 2);
    }

    #[test]
    fn try_into_inner_needs_the_only_handle() {
        let shared = SharedTable::new(table());
        let clone = shared.clone();
        assert!(shared.clone().try_into_inner().unwrap().is_none());
        drop(clone);
        let owned = shared.try_into_inner().unwrap();
        assert!(owned.is_some());
    }

    /// A poisoned lock raises the crate's typed error instead of handing out a
    /// possibly-inconsistent table.
    #[test]
    fn a_poisoned_lock_is_reported_not_swallowed() {
        let shared = SharedTable::new(table());
        let poisoner = shared.clone();
        let panicked = std::thread::spawn(move || {
            let _ = poisoner.with_write(|_| panic!("poison the lock"));
        })
        .join();
        assert!(panicked.is_err(), "the writer thread must have panicked");
        assert!(shared.read().is_err(), "a poisoned read must raise");
        assert!(shared.write().is_err(), "a poisoned write must raise");
        // `id` is not part of the mutable state a panicking writer could have
        // half-written, so it still answers (documented in `SharedTable::id`).
        assert_eq!(shared.id(), shared.id());
    }
}
