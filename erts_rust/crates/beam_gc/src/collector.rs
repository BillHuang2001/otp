//! The two-generation collector: [`Collector`], [`Collection`] and [`CollectionKind`].
//!
//! A **minor** collection Cheney-copies every live young term straight into the
//! `mature` space and resets the nursery (immediate promotion); a **major**
//! (fullsweep) collection evacuates `young` and `mature` into one freshly-sized space.
//! Because terms are immutable, a mature object can never refer to a later-allocated
//! young object, so mature objects are never rescanned — the generational payoff.
//! The driving logic here is safe Rust; the word-level copier lives in
//! [`crate::copy`].

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::copy::Copier;
use crate::policy::HeapPolicy;
use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;

/// Which collection a [`Collector`] ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    /// A young-only collection: the nursery is evacuated into the mature space.
    Minor,
    /// A fullsweep collection: young and mature are evacuated into a fresh space.
    Major,
}

/// The result of one [`Collector::collect`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collection {
    /// Which kind of collection ran.
    pub kind: CollectionKind,
    /// The statistics of that collection.
    pub stats: GcStats,
}

/// The two-generation collector owning the young and mature spaces.
pub struct Collector {
    policy: HeapPolicy,
    young: HeapSpace,
    mature: HeapSpace,
    minor_since_major: u32,
    stats: GcStats,
}

impl Collector {
    /// Creates a collector with the given `policy`, using the default allocator.
    pub fn new(policy: HeapPolicy) -> Result<Collector> {
        Self::with_allocator(beam_alloc::registry::default_allocator().clone(), policy)
    }

    /// Creates a collector with the given `policy`, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, policy: HeapPolicy) -> Result<Collector> {
        let young = HeapSpace::with_allocator(alloc.clone(), policy.initial_young_words)?;
        let mature = HeapSpace::with_allocator(alloc, policy.initial_mature_words)?;
        Ok(Collector {
            policy,
            young,
            mature,
            minor_since_major: 0,
            stats: GcStats::default(),
        })
    }

    /// The collector's heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        &self.policy
    }

    /// The young (nursery) space.
    pub fn young(&self) -> &HeapSpace {
        &self.young
    }

    /// The young (nursery) space, mutably — where the mutator allocates.
    pub fn young_mut(&mut self) -> &mut HeapSpace {
        &mut self.young
    }

    /// The mature space.
    pub fn mature(&self) -> &HeapSpace {
        &self.mature
    }

    /// The collector's accumulated statistics.
    pub fn stats(&self) -> GcStats {
        self.stats
    }

    /// Whether the nursery can no longer satisfy `need_words` more words.
    pub fn should_collect(&self, need_words: usize) -> bool {
        self.young.available_words() < need_words
    }

    /// Runs a collection, picking minor vs major by policy and escalating a
    /// would-not-fit minor to a major.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let kind = if self.minor_since_major >= self.policy.fullsweep_after
            || self.mature.available_words() < self.young.used_words()
        {
            CollectionKind::Major
        } else {
            CollectionKind::Minor
        };
        let stats = match kind {
            CollectionKind::Minor => self.collect_minor(roots)?,
            CollectionKind::Major => self.collect_major(roots)?,
        };
        Ok(Collection { kind, stats })
    }

    /// Runs a minor collection: evacuate the nursery's live terms into the mature space.
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let mut from = FromSet::new();
        from.add(&self.young);
        // The destination must fit the whole nursery up front (immediate promotion, no
        // partial-copy rollback); `collect` escalates to a major when it would not.
        let stats = Copier::new(&from, &mut self.mature).run(roots)?;
        self.young.reset();
        self.minor_since_major += 1;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.minor_collections += 1;
        Ok(stats)
    }

    /// Runs a major (fullsweep) collection: evacuate young and mature into a fresh space.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let mut from = FromSet::new();
        from.add(&self.young);
        from.add(&self.mature);
        let live = self.young.used_words() + self.mature.used_words();
        let size = self
            .policy
            .size_after_fullsweep(live, self.policy.need_reserve_words);
        let alloc = self.young.allocator().clone();
        // Sized up front so the copy always fits — a collection never leaves the
        // from-space half-forwarded.
        let mut fresh = HeapSpace::with_allocator(alloc, size)?;
        let stats = Copier::new(&from, &mut fresh).run(roots)?;
        self.mature = fresh; // drops the old mature space
        self.young.reset();
        self.minor_since_major = 0;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.major_collections += 1;
        Ok(stats)
    }
}
