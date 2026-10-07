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
use crate::heap_info::HeapObservable;
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

    /// The number of minor collections run since the last fullsweep — the C `GEN_GCS`, and
    /// the value the `garbage_collection` item reports as `minor_gcs`.
    pub fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// Whether the nursery can no longer satisfy `need_words` more words.
    pub fn should_collect(&self, need_words: usize) -> bool {
        self.young.available_words() < need_words
    }

    /// Runs a collection, picking minor vs major by policy and escalating a
    /// would-not-fit minor to a major.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let kind = if self.policy.fullsweep_due(self.minor_since_major)
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
        let used = self.young.used_words();
        self.young.reset();
        self.minor_since_major += 1;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.minor_collections += 1;
        // The nursery is empty again: size it for the next round (the C's minor-time
        // young-heap resize, `erl_gc.c:1493`).
        self.resize_nursery(used)?;
        Ok(stats)
    }

    /// Runs a major (fullsweep) collection: evacuate young and mature into a fresh space.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let mut from = FromSet::new();
        from.add(&self.young);
        from.add(&self.mature);
        let used = self.young.used_words();
        let live = used + self.mature.used_words();
        // Size the fresh space from the heap policy: grow, shrink or keep the current
        // mature capacity (`HeapPolicy::mature_after_fullsweep`, the C
        // `adjust_after_fullsweep`).
        let size = self
            .policy
            .mature_after_fullsweep(self.mature.capacity_words(), live, 0);
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
        self.resize_nursery(used)?;
        Ok(stats)
    }

    /// Sizes the (now empty) nursery for the next round from the heap-size policy — the
    /// C's `next_heap_size(p, stack_size + MAX(size_before, need), 0)` applied to the
    /// nursery (`erl_gc.c:1493`). A nursery that filled up grows, one left mostly empty
    /// shrinks, and one in between keeps its size; the space is only replaced when the
    /// target differs, so a steady-state workload pays nothing.
    fn resize_nursery(&mut self, used_words: usize) -> Result<()> {
        let capacity = self.young.capacity_words();
        let target = self.policy.young_after_collection(capacity, used_words);
        if target != capacity {
            let alloc = self.young.allocator().clone();
            self.young = HeapSpace::with_allocator(alloc, target)?;
        }
        Ok(())
    }
}

impl HeapObservable for Collector {
    /// Minors since the last fullsweep.
    fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// The nursery (the "heap" in the C sense) capacity, in words.
    fn heap_size_words(&self) -> usize {
        self.young.capacity_words()
    }

    /// The nursery plus the mature generation (the C's `heap_sz + old heap`), in words.
    fn total_heap_size_words(&self) -> usize {
        self.young.capacity_words() + self.mature.capacity_words()
    }

    fn heap_policy(&self) -> &HeapPolicy {
        &self.policy
    }
}