// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! **Region reclamation** — dropping the parts of the runtime's two never-freed term regions that
//! provably hold no live word, and (Phase 1b) **evacuating** the live words of a partially live
//! store region so it can be dropped too.
//!
//! # The two regions, and why they grow without bound
//!
//! [`Runtime::eval_heap`] (the `-eval`/table-less copy-out **store**) and [`Runtime::arena`] (the
//! engine `MinimalContext` every table-less `alloc_words` lands on) are both historically
//! **pointer-stable**: a term on them stays valid until this pass releases it, so a raw word handed
//! to a caller is sound. Neither one reclaims anything on its own:
//!
//! * **the store** is a [`beam_alloc::Heap`]: a bump arena whose *replacement* growth
//!   ([`Runtime::grow_eval_heap`]) installs a doubled region and pushes the replaced one onto
//!   [`Runtime::eval_heap_retired`]. A replaced region is ~full, so it is never rewritable, and it
//!   is never dropped on its own — what the store retains is the **total volume it ever copied**,
//!   not its live set. (`Heap::reset_to` rewinds the *current* region's bump pointer but releases
//!   no RSS; only a `Heap`'s `Drop` frees a region's carrier.)
//! * **the arena** is `beam_engine`'s `ContextHeap`: a ladder of reservations that only ever grows.
//!   It has no `Drop`-on-rewind either — but it *does* expose the two doors a caller needs, which
//!   this module is the first production caller of: [`MinimalContext::heap_mark`],
//!   [`MinimalContext::for_each_heap_region`] and [`MinimalContext::heap_rewind`].
//!
//! # One descent, three actions
//!
//! A pass asks a single question of the term graph, once: **does any reachable term stand inside
//! any of these address ranges?** The ranges cover exactly the words a pass may touch:
//!
//! * the store's **current** region above this window's cursor (`[cursor, used)`), and every
//!   retired region the window created (`eval_heap_retired[baseline..]`);
//! * every arena region at or above the mark taken when the window opened (the marked region's
//!   `[mark.used, used)` and every newer region in full).
//!
//! Then three independent actions follow from the answer:
//!
//! * **arena** — *Tier A*: no range hit means nothing reachable stands at or above the mark, which
//!   is exactly [`MinimalContext::heap_rewind`]'s caller contract, so the arena collapses to the
//!   mark (dropping every newer region, whose carrier `Drop` frees, and rewinding the surviving
//!   frontier).
//! * **store, whole-region** — a *whole-dead region is dropped, no word is moved*: a retired region
//!   with no hit leaves the `Vec` (its `Drop` frees the carrier, so RSS really falls), and the
//!   current region with no hit in `[cursor, used)` is `reset_to(cursor)`.
//! * **store, move** ([`compact`]) — the *interleaved* case the whole-region action cannot help:
//!   when the candidates are only **partially** live (each retaining one live word and therefore
//!   held whole) and their dead fraction is worth reclaiming, every word reachable from the
//!   runtime's **mutable** root corpus is evacuated into one fresh region sized by the store's own
//!   growth policy ([`Runtime::eval_store_growth_capacity`]) and the sources are dropped. That is
//!   the one place a store word moves; the corpus, the post-move verification, the thresholds and
//!   the decline list are [`compact`]'s own documentation.
//!
//! When the move fires, the whole-region action is skipped: there is no candidate left to classify.
//! When it is skipped, or it declines **before the first word moves**, the whole-region action runs
//! exactly as it always did, so the cheap case never depends on the expensive one. A refusal *after*
//! a word moved ([`compact::MoveOutcome::KeptBoth`]) suppresses it instead: the descent that just
//! failed is known to be incomplete for those very ranges, so classifying them on it would free a
//! region whose live word the verification had just found.
//!
//! # This is where a `-eval` compile's 4.63 GB goes
//!
//! A `compile:file/2` in process is **one** top-level `-eval` statement, so the statement window's
//! between-statements rewind (`crate::eval::scratch`) never gets a second boundary: measured, a
//! tree-compiler `inet_parse` compile ends with **19** store regions / 620 756 992 words (4.63 GB,
//! fully written and resident) and `resets == 0`. At the **driver exit** every one of those
//! replaced regions is dead — the compile's result is the run's final value, standing in the
//! current region — so a single descent over the exit's root set classifies all 18 retired regions
//! as droppable and the chain collapses without moving a single word. That is the whole-region
//! action's point: the biggest measured retention in the runtime needs no move at all, only a
//! proof. What survives that pass is the *partially* live remainder — one retired region retained
//! on evidence, and the current region holding the result — which is the shape the move exists for.
//!
//! # The completeness precondition (what makes a drop sound)
//!
//! Dropping a region frees its words. A word is *live* when something the runtime cannot reach
//! from its root enumeration names it; so the enumeration must be **complete**, and this module
//! states exactly how it is and where it is not.
//!
//! 1. **The value root set** is [`Runtime::gather_off_heap_roots`] (the same enumeration the
//!    statement window's own rewind rests on) plus the caller's own live terms — the evaluator's
//!    [`Env`](crate::eval::Env) values at a boundary, and at the exit those plus the driver's
//!    result (or the raise's reason term). The descent is
//!    [`crate::eval::reclaim::walk`](crate::eval::reclaim::walk) — the *one* walker — with
//!    [`RangeProbe`] as its sink.
//! 2. **Every holder the value gather leaves out is a store the writer pinned.** The families the
//!    gather deliberately skips (`persistent_term`, trace match specs, `port_set_data` payloads,
//!    armed select payloads, the runner's recorded outcomes, a process's async-GC request id, the
//!    port layer's queued events) are all written through
//!    [`Runtime::stabilize_term`]/[`Runtime::stabilize_term_with`], which latches the window. So
//!    the window's **pin** is the compensating guard, and it is honoured — see the decline list.
//! 3. **The pin is classified.** A pin that names a *runtime-lifetime* store (the
//!    `stabilize_term` family and `decode_caller_tag`) outlives the window: its word is still
//!    named after the driver returns, so it declines **both** sites. A pin that names a *machine
//!    in flight* (`pin_window_if_store_written`, `apply_eval_closure`) is dead by the time the
//!    driver returns — the machine that held those roots was dropped inside the statement — so it
//!    declines the boundary and not the exit. That distinction is what makes the exit-site pass
//!    reachable at all: a compile pins the window on its **first** store-writing nested run, so a
//!    transient pin honoured at the exit would disable the one case this module exists for.
//!
//!    A lifetime pin is honoured twice, because the statement window's own latch does not survive
//!    its own decline (a declined window's mark jumps to the store's frontier and its latches are
//!    cleared — correct for that window, but it would leave this pass's lagging cursor free to
//!    reset or drop the very region the pin just took a word in). [`note_lifetime_store`]
//!    therefore registers the region **with this pass**: the current region may not be reset for
//!    the rest of its life as the current region, and every region retired from then on is
//!    excluded from the retired candidates. Both are lifted by the next growth of the store.
//!
//! 3b. **The current region is shared with the statement window.** That window's mark only ever
//!    advances, so it is this pass's **floor** ([`raise_store_floor`]): a reset below it would
//!    leave the window pointing above the store's frontier and its next boundary could prove
//!    nothing. The two mechanisms therefore always agree about which words the current region has
//!    already been accounted for.
//! 4. **Only words written inside the window are candidates.** The store's retired candidates are
//!    `eval_heap_retired[baseline..]` where `baseline` was recorded at [`open`], and the current
//!    region's candidate range starts at this pass's cursor — never at the region base. Words
//!    below those marks were written **before** this window opened, by an earlier `-eval` run
//!    whose own pin is long gone (a `persistent_term:put` from the previous run is *not* a root
//!    family and *not* pinned any more), so the pass must never touch them. That is why the
//!    current region is reset to its **cursor** and not to `0`: `reset_to(0)` would need a proof
//!    about words this pass cannot enumerate.
//! 5. **Doubt ⇒ decline.** A walk that cannot finish (visit budget, unknown boxed header), a
//!    machine in flight, a refused rewind — each records a `&'static str` and leaves both regions
//!    exactly as they were. A **recorded raise is not doubt**: its live reason word is read out of
//!    [`RunnerState`](crate::runner::RunnerState) by
//!    [`RunnerState::for_each_error_live_reason`](crate::runner::RunnerState::for_each_error_live_reason)
//!    and pushed into the value root set, so a region holding one is simply *retained* by the same
//!    descent as any other live word. Reclaiming the regions a pending error names costs nothing in
//!    soundness and is often the only reclaimable state there is: the map holds exactly
//!    finished-and-not-yet-reported pids, which on the `-eval` route is the run's own last failure.
//!    (A **moving** mechanism cannot lean on that accessor — it is read-only, and
//!    [`beam_common::Raise`] exposes no mutable slot — so the move refuses a region a recorded
//!    reason **stands in**, checked by address; see [`compact::attempt_move`].)
//!
//! # Why Tier A and not Tier B
//!
//! `beam_engine` also offers `reclaim_heap` (Tier B), which *copies* the live graph into a fresh
//! survivor. It is deliberately not used for the arena:
//!
//! * its `forward` closure is **fallible**, and an `Err` from it leaves the arena **empty** with
//!   the epoch bumped and the caller's roots abandoned — an unrecoverable state, documented at
//!   `beam_engine/src/context/reclaim.rs`'s `reclaim_heap`;
//! * Tier A's refusal path is clean: `ContextHeap::rewind` validates the epoch, the region index,
//!   the base address and the used figure **before it mutates anything**, so an `Err` leaves the
//!   arena byte-identical. A refusal is therefore a decline, never an error to propagate.
//!
//! The **store** move is a different matter: its live words are the ones that are still needed,
//! and `beam_gc` is the crate that owns relocation — so the move drives
//! [`beam_gc::Relocator`](compact) directly rather than the arena's Tier B door. The two terms
//! that are exempt from relocation inside a `-eval` run are *process-heap* terms the evaluator
//! keeps in Rust locals; the store move forwards those very slots instead
//! ([`compact::MoveSlots`]), which is what makes it compatible with the exemption.
//!
//! # Off-heap payloads, `Visited`, and `Frontier` — three non-issues
//!
//! * **Refc payload handles survive a region drop *and* a move.** A `[BinRef][ErlSubBits]` pair is
//!   on-heap, but the payload it names lives in [`Runtime::eval_off_heap`], whose handles are
//!   released by the escaped-payload prune (`crate::eval::reclaim::prune`), which derives
//!   reachability itself from [`Runtime::for_each_off_heap_region`] and
//!   [`Runtime::gather_off_heap_roots`]. Dropping a region removes on-heap pairs only; it never
//!   removes a handle, so it can neither orphan nor over-release a payload. A moved pair keeps its
//!   payload pointer (`BinRef::val` is not a heap word, so the copier treats the `BinRef` as a
//!   leaf), and the destination — `eval_heap`, or a retired region on a post-copy refusal — is
//!   covered by [`Runtime::for_each_off_heap_region`]. A dropped region is never "the last namer
//!   of a payload" in the prune's terms, because the prune answers from its own descent, not from
//!   this one.
//! * **`src/context/gc_walk.rs`'s `Visited` set needs no reset.** Its validity domain is a single
//!   walk: `Walk::begin` bumps a generation counter (`Visited::begin`, `O(1)`) and every probe
//!   compares against it, so no stored address outlives the walk that stored it. A region dropped
//!   between two collections therefore cannot leave a stale address that makes a later collection
//!   skip a live object.
//! * **`arena_census`'s window arithmetic is untouched.** `src/runtime/arena_census.rs` folds
//!   [`Runtime::alloc_words`]'s *served requests* (`record_alloc`) against a thread-local window;
//!   `heap_rewind` lowers `beam_engine`'s live-word figure, never a served request, so an in-flight
//!   window's delta stays monotone and the report keeps working. (`crate::eval::reclaim::resource`'s
//!   `Frontier` is the *minted-term* frontier, not the arena's.)
//!
//! # Cost
//!
//! A pass with no candidate range costs one `Vec` size test. A pass with candidates pays one
//! descent over the live set plus `O(regions)`; the descent stops as soon as *every* range has its
//! answer (`Reach::done`), which for a mostly-live store is the first few words.
//!
//! A **move** costs one exact count walk over the same corpus, one copy of the live set, one
//! destination scan and one verification descent — plus, on a refusal after the first copy, one
//! extra region that is kept rather than dropped. It is decided only when half the candidates are
//! dead, so a mostly-live store never pays it.
//!
//! # The switch
//!
//! [`RegionReclaimConfig`] has three independent bits (store, the store **move**, arena), all on by
//! default, latched in [`Runtime::new`] from `BEAM_RT_REGION_RECLAIM`
//! (`all`/`store`/`arena`/`nomove`/`off`) and settable with
//! [`Runtime::set_region_reclaim_enabled`]. The switch can only *disable* an action; it never
//! disables a correctness guard (every decline below is decided independently of it), and it
//! changes no rooting, no liveness and no reclamation-timing decision anywhere else.
//!
//! The pass's two **sites** are the statement window's: [`open`] runs from
//! [`crate::eval::scratch::open`], and a pass runs from
//! [`crate::eval::scratch::boundary`](crate::eval::scratch::boundary) and
//! [`crate::eval::scratch::close`](crate::eval::scratch::close). With the *pre-existing*
//! diagnostic `Runtime::set_eval_scratch_enabled(false)` no window opens, so neither site exists —
//! it is a measurement switch, never a production one.

use core::mem::size_of;

use beam_engine::HeapMark;
use beam_term::Eterm;

use super::{RootFamily, Runtime};
use crate::eval::reclaim::{self, Walk};
use crate::eval::scratch::{RangeProbe, WindowState};

pub(crate) mod compact;
pub(crate) mod holder;
pub(crate) use compact::{MoveSlots, MoveOutcome};

/// Which of the runtime-owned regions a pass may act on.
///
/// Three independent bits, all **on** by default. See [`Runtime::set_region_reclaim_enabled`] and
/// the module docs' *The switch*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionReclaimConfig {
    /// Whether the `-eval` term store's dead regions may be dropped
    /// ([`Runtime::eval_heap`] + [`Runtime::eval_heap_retired`]).
    pub store: bool,
    /// Whether a **partially** live store region may be evacuated into a fresh region
    /// ([`compact`]) — the store half's copy, without which a region holding one live word is
    /// retained whole.
    ///
    /// Independent of [`store`](RegionReclaimConfig::store): the whole-region drop runs whether or
    /// not a move is allowed, and a move is only ever attempted when that bit is also set.
    pub store_move: bool,
    /// Whether the engine arena's dead tail may be rewound ([`Runtime::arena`]).
    pub arena: bool,
}

impl RegionReclaimConfig {
    /// Every action off — the A/B baseline a measurement compares against.
    pub const OFF: RegionReclaimConfig = RegionReclaimConfig {
        store: false,
        store_move: false,
        arena: false,
    };

    /// Whether at least one action is enabled.
    pub const fn any(&self) -> bool {
        self.store || self.arena
    }

    /// The configuration the `BEAM_RT_REGION_RECLAIM` environment gate selects.
    ///
    /// `all` (the default, also what an **unrecognised** value yields), `store` (the whole-region
    /// store action **and** its move), `store-whole` (the store action without the move), `arena`,
    /// `nomove` (everything, minus the move) and `off`. The gate exists so a measurement can hold
    /// one action out of the picture on the same binary, like the two pre-existing diagnostic
    /// switches; it can only turn an action **off**.
    pub(crate) fn from_env() -> RegionReclaimConfig {
        match std::env::var("BEAM_RT_REGION_RECLAIM") {
            Ok(value) => RegionReclaimConfig::parse(value.trim()),
            Err(_) => RegionReclaimConfig::default(),
        }
    }

    /// The named configuration, or the default for anything unrecognised.
    fn parse(value: &str) -> RegionReclaimConfig {
        match value {
            "off" | "none" | "0" | "false" => RegionReclaimConfig::OFF,
            "store" => RegionReclaimConfig {
                store: true,
                arena: false,
                ..RegionReclaimConfig::default()
            },
            "store-whole" => RegionReclaimConfig {
                store: true,
                store_move: false,
                arena: false,
            },
            "arena" => RegionReclaimConfig {
                store: false,
                arena: true,
                ..RegionReclaimConfig::default()
            },
            "nomove" => RegionReclaimConfig {
                store_move: false,
                ..RegionReclaimConfig::default()
            },
            _ => RegionReclaimConfig::default(),
        }
    }
}

impl Default for RegionReclaimConfig {
    fn default() -> RegionReclaimConfig {
        RegionReclaimConfig {
            store: true,
            store_move: true,
            arena: true,
        }
    }
}

/// What the region-reclaim pass has done, as [`Runtime::region_reclaim_stats`] publishes it.
///
/// Every field is a **monotone counter** of decisions taken; none is a sampled figure, and none is
/// adjusted by reading it.
///
/// [`arena_words`](crate::runtime::RetentionStats::arena_words) keeps its **cumulative demand**
/// meaning — a rewind does not lower it — while the arena's *live* figure is
/// `beam_engine::ContextHeapStats::live_words` (its `released_words` is the falling counterpart).
/// `eval_heap_retired_regions`/`eval_heap_retired_words` on the store's side are the figures a
/// store drop moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegionReclaimStats {
    /// How many times a store region was **compacted** — its live words moved into a fresh region
    /// and the sources dropped ([`compact`]).
    ///
    /// Zero while the move never fires: a store region whose live fraction is too high, whose
    /// candidates are below the threshold, or whose pass declined is retained whole, exactly as the
    /// whole-region action leaves it.
    pub store_compactions: u64,
    /// How many store words the drops reclaimed, summed.
    ///
    /// A dropped retired region contributes its whole written prefix; the current region
    /// contributes the window prefix a reset released; a move contributes its candidates' **dead**
    /// words (`total - live`), which is exactly what dropping the sources frees.
    pub store_words_reclaimed: u64,
    /// How many store regions were dropped (each frees its region's carrier).
    ///
    /// A whole-dead retired region, and — for a move — every retired candidate plus the old current
    /// region when its below-cursor prefix is empty.
    pub store_regions_dropped: u64,
    /// How many **move** attempts declined, for any reason in [`compact`]'s decline list.
    ///
    /// Kept separate from [`reclaim_declines`](RegionReclaimStats::reclaim_declines): the move is an
    /// optional extra action, so a moved-off A/B run (or a store whose live fraction never justifies
    /// a copy) must not read as if the whole pass had refused.
    pub store_move_declines: u64,
    /// The reason of the most recent move decline.
    pub store_move_last_decline: Option<&'static str>,
    /// How many times the arena was rewound.
    pub arena_reclaims: u64,
    /// How many arena words the rewinds released, summed
    /// (`beam_engine::HeapRewind::words_released`).
    pub arena_words_reclaimed: u64,
    /// How many arena regions the rewinds dropped, summed
    /// (`beam_engine::HeapRewind::regions_dropped`).
    pub arena_regions_dropped: u64,
    /// How many passes declined because soundness could not be shown.
    pub reclaim_declines: u64,
    /// The reason of the most recent decline — see the module docs' decline list.
    pub reclaim_last_decline: Option<&'static str>,
    /// The raw **header word** of the most recent *unknown-header* refusal: the walk read it as a
    /// boxed object's first word, and its subtag (`header & beam_term::tag::TAG_HEADER_MASK`) is one
    /// no constructor in this workspace writes.
    ///
    /// Two walks can refuse on it, and this is the one field that covers both: the **pass's own**
    /// descent (which records the same refusal in [`reclaim_last_decline`]) and — after a word has
    /// already moved — the store move's **post-move verification**, whose refusal is counted on the
    /// move's own [`store_move_declines`](RegionReclaimStats::store_move_declines) /
    /// [`store_move_last_decline`](RegionReclaimStats::store_move_last_decline) pair. It is `None`
    /// after any other refusal (and until the first one), so a run reads the reason and the header
    /// together instead of guessing which of the two walks refused.
    pub reclaim_last_decline_header: Option<usize>,
    /// What the store move's **post-move verification** hit first reproduced in — the holder that
    /// names a live word inside a store region the move had just evacuated, which is why
    /// [`store_compactions`](RegionReclaimStats::store_compactions) did not count that attempt and
    /// every source region was kept ([`compact::MoveOutcome::KeptBoth`]).
    ///
    /// `None` until a verification first hit. [`MoveHitOwner::OutsideCorpus`] is itself a finding:
    /// no slot of the move's corpus ([`RootFamily`] families plus the caller's [`MoveSlots`])
    /// reproduces the hit, so the holder is a store the corpus does not enumerate.
    pub store_move_hit: Option<MoveHitOwner>,
}

/// Which slot of the move's corpus the post-move verification's first hit reproduced in.
///
/// The verification re-walks the corpus **after** the copy, so a holder that was rewritten names the
/// destination and only a holder the corpus cannot rewrite still names a source range. This names
/// where the reproducing slot lives — the diagnostic a fix needs, because each variant answers a
/// different question: a [`Family`](MoveHitOwner::Family) slot means the family walk reached it (so
/// the move's forwarding skipped or mis-copied it); a
/// [`CallerSlot`](MoveHitOwner::CallerSlot) means the caller's own slot corpus did; and
/// [`OutsideCorpus`](MoveHitOwner::OutsideCorpus) means neither does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveHitOwner {
    /// A family of `Runtime::for_each_root_family_mut`, at this slot index within its slice.
    Family(RootFamily, usize),
    /// The caller's own slot corpus (`MoveSlots`), at this index.
    CallerSlot(usize),
    /// No slot in the corpus reproduces the hit — the holder is outside it.
    OutsideCorpus,
}

impl MoveHitOwner {
    /// The root family a [`MoveHitOwner::Family`] owner names, `None` for the other two shapes.
    pub fn family(&self) -> Option<RootFamily> {
        match self {
            MoveHitOwner::Family(family, _) => Some(*family),
            MoveHitOwner::CallerSlot(_) | MoveHitOwner::OutsideCorpus => None,
        }
    }

    /// The slot index within [`family`](MoveHitOwner::family)'s slice — or, for
    /// [`MoveHitOwner::CallerSlot`], within the caller's own slot list. `None` for
    /// [`MoveHitOwner::OutsideCorpus`], which reproduces in no slot at all.
    pub fn slot(&self) -> Option<usize> {
        match self {
            MoveHitOwner::Family(_, slot) | MoveHitOwner::CallerSlot(slot) => Some(*slot),
            MoveHitOwner::OutsideCorpus => None,
        }
    }
}

impl RegionReclaimStats {
    /// The [`RootFamily`] of [`store_move_hit`](RegionReclaimStats::store_move_hit) when the hit
    /// reproduced in a family slot; `None` for a caller slot, for `OutsideCorpus` and until a hit.
    pub fn store_move_hit_family(&self) -> Option<RootFamily> {
        self.store_move_hit.and_then(|owner| owner.family())
    }

    /// The slot index [`store_move_hit`](RegionReclaimStats::store_move_hit) reproduced in — within
    /// the [family](RegionReclaimStats::store_move_hit_family)'s slice, or within the caller's own
    /// slot list for a [`MoveHitOwner::CallerSlot`] hit; `None` for `OutsideCorpus` and until a hit.
    pub fn store_move_hit_slot(&self) -> Option<usize> {
        self.store_move_hit.and_then(|owner| owner.slot())
    }
}

/// Where a pass runs.
///
/// The two sites differ in **one** decision — whether a *transient* (machine-in-flight) pin
/// declines — see the module docs' *completeness precondition*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Site {
    /// The boundary **before** a top-level `-eval` statement.
    Boundary,
    /// The `-eval` driver's exit, with the driver's own live terms handed over.
    Exit,
}

impl Site {
    /// Whether a transient pin (a machine in flight) is a decline here.
    ///
    /// It is only meaningful at the boundary: a machine that pinned the window was dropped inside
    /// the statement, so at the exit its roots name nothing.
    fn honours_transient_pins(&self) -> bool {
        matches!(self, Site::Boundary)
    }
}

/// One pass's input, assembled by the statement window.
pub(crate) struct PassInput<'a> {
    /// Where the pass runs.
    pub(crate) site: Site,
    /// What the open statement window reports (opened-ness and its pin latches).
    pub(crate) window: WindowState,
    /// The caller's own live terms — the evaluator's `Env` values, plus the driver's result at
    /// the exit. Added to [`Runtime::gather_off_heap_roots`] to form the descent's root set.
    ///
    /// These are **copies**, and the move ignores them: [`compact::attempt_move`] forwards — and
    /// sizes its destination from — the real slots ([`MoveSlots`]), which are handed to [`pass`]
    /// separately.
    pub(crate) value_roots: &'a [Eterm],
    /// `Some(reason)` when the caller knows it holds a live term it **cannot** hand over (the
    /// `-eval` driver's `Flow::Fatal`: a `BeamError` may carry a live reason word and exposes no
    /// mutable accessor). Declines the whole pass.
    pub(crate) withheld: Option<&'static str>,
}

/// The state one open window lends a pass: the frontiers it may reclaim down to.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    /// The arena's mark at [`open`]. A rewind may drop everything at or above it.
    arena_mark: HeapMark,
    /// The store's current-region base address at [`open`] (or at the last advance).
    store_base: usize,
    /// The store's current-region `used` frontier at [`open`] (or at the last advance).
    store_mark: usize,
    /// `eval_heap_retired.len()` at [`open`]: every retired region at or after this index was
    /// created **inside** the window, so its words were written inside the window.
    store_retired_baseline: usize,
}
/// The region-reclaim pass's per-runtime state.
///
/// Always present on the [`Runtime`]; `cursor` is `Some` only between [`open`] and the end of the
/// `-eval` driver run that opened it. Every field's meaning is in the module docs.
#[derive(Debug, Clone)]
pub(crate) struct RegionReclaim {
    /// The three action bits ([`Runtime::set_region_reclaim_enabled`]).
    config: RegionReclaimConfig,
    /// The open window's frontiers, if a pass may run.
    cursor: Option<Cursor>,
    /// The published counters.
    stats: RegionReclaimStats,
}

impl Default for RegionReclaim {
    fn default() -> RegionReclaim {
        RegionReclaim {
            config: RegionReclaimConfig::default(),
            cursor: None,
            stats: RegionReclaimStats::default(),
        }
    }
}

impl RegionReclaim {
    /// The configuration the environment gate selected — parsed **once**, in [`Runtime::new`].
    pub(crate) fn from_env() -> RegionReclaim {
        RegionReclaim {
            config: RegionReclaimConfig::from_env(),
            ..RegionReclaim::default()
        }
    }

    /// The three action bits in force.
    pub(crate) fn config(&self) -> RegionReclaimConfig {
        self.config
    }

    /// Replaces the three action bits. The switch can only disable an action.
    pub(crate) fn set_config(&mut self, config: RegionReclaimConfig) {
        self.config = config;
    }

    /// The published counters.
    pub(crate) fn stats(&self) -> RegionReclaimStats {
        self.stats
    }

    /// Records a decline: one counter plus the reason, and no region touched.
    ///
    /// It also clears [`RegionReclaimStats::reclaim_last_decline_header`], because the header
    /// describes *this* refusal: a refusal that walked the graph and met no unknown header must not
    /// leave the previous one's header standing beside its reason.
    fn decline(&mut self, reason: &'static str) {
        self.stats.reclaim_declines = self.stats.reclaim_declines.saturating_add(1);
        self.stats.reclaim_last_decline = Some(reason);
        self.stats.reclaim_last_decline_header = None;
    }

    /// Records a **walk** verdict's refusal: the reason string every site has always recorded
    /// ([`Walk::reason`]) plus, when that is what the walk met, the unknown header word
    /// ([`Walk::header`]).
    fn decline_walk(&mut self, verdict: &Walk) {
        self.decline(verdict.reason());
        self.note_unknown_header(verdict);
    }

    /// Records the unknown header a walk refused on, without touching the decline counters, and
    /// traces it — the header word, its subtag and the term word that led there are the three facts
    /// a one-line summary cannot carry, and a `[rt-mem]` line costs nothing unless the channel is on.
    ///
    /// Two walks reach it: this pass's own descent (whose reason is recorded by [`decline_walk`])
    /// and a store move's **post-move verification**, whose refusal is counted on the move's own
    /// [`RegionReclaimStats::store_move_declines`] and whose reason is
    /// [`RegionReclaimStats::store_move_last_decline`]. The header field is the one place both meet,
    /// which is what lets a single run report the move's hit and the header it met.
    fn note_unknown_header(&mut self, verdict: &Walk) {
        let Walk::UnknownHeader { header, term } = verdict else {
            return;
        };
        self.stats.reclaim_last_decline_header = Some(*header);
        crate::debug_trace::mem(format_args!(
            "reclaim: the reachability walk met an unknown boxed header={header:#x} subtag={:#04x} \
             term={term:#x}",
            header & beam_term::tag::TAG_HEADER_MASK
        ));
    }

    /// Records what the store move's post-move verification hit first reproduced in
    /// ([`RegionReclaimStats::store_move_hit`]).
    fn note_store_move_hit(&mut self, owner: MoveHitOwner) {
        self.stats.store_move_hit = Some(owner);
    }

    /// Records a **move-only** decline on the move's own counters ([`compact`]'s decline list).
    fn decline_move(&mut self, reason: &'static str) {
        self.stats.store_move_declines = self.stats.store_move_declines.saturating_add(1);
        self.stats.store_move_last_decline = Some(reason);
    }
}

/// The address of a region's first word, derived from its bump pointer and its frontier.
///
/// `hp - used` is the base **whether or not** the region holds a word, which is why it is derived
/// this way rather than from [`beam_alloc::Heap::used_words_mut`]: that slice is empty for a
/// region holding nothing, and an empty slice's data pointer is dangling, so it would answer a
/// different "base" for an empty region than for the same region once it holds a word.
/// [`crate::eval::scratch`]'s window uses the identical derivation.
pub(crate) fn heap_region_base(heap: &beam_alloc::Heap) -> usize {
    (heap.hp() as usize).saturating_sub(heap.used() * size_of::<Eterm>())
}

/// Opens the region-reclaim window for one `-eval` driver run.
///
/// Called by [`crate::eval::scratch::open`] immediately after it opens the statement window, so
/// the two windows share one frontier. A no-op when a window is already open (a nested `-eval`
/// inherits the outer one's frontiers, which is exactly right).
pub(crate) fn open(runtime: &mut Runtime) {
    if runtime.region_reclaim.cursor.is_some() {
        return;
    }
    let cursor = Cursor {
        arena_mark: runtime.arena.heap_mark(),
        store_base: heap_region_base(&runtime.eval_heap),
        store_mark: runtime.eval_heap.used(),
        store_retired_baseline: runtime.eval_heap_retired.len(),
    };
    runtime.region_reclaim.cursor = Some(cursor);
}

/// Closes the region-reclaim window. Called by [`crate::eval::scratch::close`] once the exit-site
/// pass has run.
pub(crate) fn close(runtime: &mut Runtime) {
    runtime.region_reclaim.cursor = None;
}

/// One region-reclaim **pass**: one descent, then the arena and/or store action.
///
/// Every refusal is recorded on [`RegionReclaimStats`] and leaves both regions exactly as they
/// were, with one exception the [`compact`] module owns and documents: a move that already copied a
/// word keeps its destination installed alongside every source region (dropping nothing), because a
/// forwarding marker in the source cannot be un-written.
///
/// `slots` is the caller's own live-term slots ([`MoveSlots`]) — the half of the move's corpus that
/// is not a runtime root family. A pass that does not move ignores it.
pub(crate) fn pass(runtime: &mut Runtime, input: PassInput<'_>, slots: &mut MoveSlots<'_>) {
    if let Some(reason) = refusal(runtime, &input) {
        runtime.region_reclaim.decline(reason);
        return;
    }
    if runtime.region_reclaim.cursor.is_none() {
        // `refusal` already declined this; kept as a typed belt-and-braces branch.
        return;
    }
    // The current region is **shared** with the statement window, whose mark only ever advances:
    // a reset below it would leave that window pointing above the store's frontier, so the pass's
    // floor is whichever of the two is higher. Adopting it is not a decline and moves no word.
    raise_store_floor(runtime);
    let Some(cursor) = runtime.region_reclaim.cursor else {
        return;
    };
    let live = live_ranges(runtime, cursor);
    if live.is_empty() {
        // Nothing was written inside the window: no work, and no descent.
        return;
    }
    let mut hits = vec![false; live.len()];
    let ranges: Vec<(usize, usize)> = live.iter().map(|candidate| candidate.range).collect();
    let roots = match gather_roots(runtime, input.value_roots) {
        Ok(roots) => roots,
        Err(reason) => {
            runtime.region_reclaim.decline(reason);
            return;
        }
    };
    {
        let verdict = reclaim::walk(&roots, &mut RangeProbe::new(&ranges, &mut hits));
        if !verdict.is_complete() {
            holder::walk_decline(runtime, &verdict, &ranges, Some(&*slots));
            runtime.region_reclaim.decline_walk(&verdict);
            return;
        }
    }
    // ---- the arena action (Tier A) ----
    let arena_candidates = live.iter().filter(|c| c.target == Target::Arena).count();
    if arena_candidates > 0 && runtime.region_reclaim.config.arena {
        let clean = live
            .iter()
            .zip(hits.iter())
            .filter(|(candidate, _)| candidate.target == Target::Arena)
            .all(|(_, hit)| !*hit);
        if clean {
            match runtime.arena.heap_rewind(cursor.arena_mark) {
                Ok(rewind) => {
                    let stats = &mut runtime.region_reclaim.stats;
                    stats.arena_reclaims = stats.arena_reclaims.saturating_add(1);
                    stats.arena_words_reclaimed = stats
                        .arena_words_reclaimed
                        .saturating_add(rewind.words_released as u64);
                    stats.arena_regions_dropped = stats
                        .arena_regions_dropped
                        .saturating_add(rewind.regions_dropped as u64);
                    // The rewind bumped the arena's mark epoch, so the old mark is dead: take the
                    // new frontier as this window's mark or every later pass would refuse.
                    if let Some(cursor) = &mut runtime.region_reclaim.cursor {
                        cursor.arena_mark = runtime.arena.heap_mark();
                    }
                }
                Err(_) => {
                    runtime
                        .region_reclaim
                        .decline("the arena refused to rewind to the window's mark");
                }
            }
        }
    }
    // ---- the store action: the move first, then the whole-dead drops ----
    if runtime.region_reclaim.config.store {
        // A move that fires leaves no candidate for the whole-region action to classify (every
        // retired candidate was dropped and the current region is the fresh destination), so the
        // two are mutually exclusive rather than layered. A move that refused **after** a word
        // moved is exclusive too, and for the opposite reason: `keep_both` kept every source region
        // precisely because the verification found a live word its corpus does not enumerate, so
        // this pass may not then classify those same regions on that same corpus.
        match compact::attempt_move(runtime, &input, slots) {
            MoveOutcome::Moved | MoveOutcome::KeptBoth => {}
            MoveOutcome::Skipped | MoveOutcome::Declined => store_action(runtime, &live, &hits),
        }
    }
    // The store may have grown inside the window since the cursor was taken; a pass never leaves a
    // cursor pointing above the frontier.
    advance_cursor(runtime);
}

/// Raises the pass's current-region floor to the statement window's mark, when the two name the
/// same region.
///
/// See [`crate::eval::scratch::ScratchState::current_region`]: the statement window's mark is the
/// frontier it has already accounted for, so it is a floor rather than a candidate range — lowering
/// the store below it would make the *next* boundary refuse "the window's mark stands above the
/// store's frontier". `O(1)`, and a no-op when no statement window is open (no `-eval` route, the
/// scratch mechanism switched off) or when a growth moved the region.
fn raise_store_floor(runtime: &mut Runtime) {
    let Some((base, mark)) = runtime.eval_scratch.current_region() else {
        return;
    };
    let Some(cursor) = &mut runtime.region_reclaim.cursor else {
        return;
    };
    if cursor.store_base == base && mark > cursor.store_mark {
        cursor.store_mark = mark;
    }
}

/// The pass's cheap preconditions — each one a recorded reason, never a guess.
fn refusal(runtime: &Runtime, input: &PassInput<'_>) -> Option<&'static str> {
    if !runtime.region_reclaim.config.any() {
        return Some("the region reclamation switch is off");
    }
    if runtime.region_reclaim.cursor.is_none() {
        return Some("no region-reclaim window is open");
    }
    if let Some(reason) = input.withheld {
        return Some(reason);
    }
    if runtime.in_machine.is_some() {
        return Some("a machine is in flight, so its roots are unpublished");
    }
    if !input.window.open {
        return Some("no statement window is open");
    }
    if input.site.honours_transient_pins() {
        if let Some(reason) = input.window.pinned {
            return Some(reason);
        }
    }
    None
}

/// The pass's **complete** root set: the caller's own live terms plus every host-side root the
/// runtime holds.
///
/// # Why this is one walk and not the value gather plus a pin
///
/// The statement window's own rewind ([`crate::eval::scratch`]) roots itself in
/// [`Runtime::gather_off_heap_roots`] — the *liveness* gather, which deliberately skips the
/// families whose words are a second copy, or already stable, or stored behind a mechanism that
/// flattens a payload ([`RootFamily::in_value_gather`]). A **runtime-lifetime** store
/// ([`Runtime::stabilize_term`]: a `persistent_term` value, a pending spawn, a timer's message, a
/// driver-emitted port term, an armed `enif_select` payload, a recorded process outcome, …) writes
/// into one of the skipped families, which is why that window's writer-side **pin** exists: it is
/// the compensating guard for a *conservative* prune that cannot enumerate the holder.
///
/// This pass has no such excuse, because it runs at exactly the two sites where the runtime is not
/// executing — the `-eval` statement boundary and the driver's exit — which is precisely the
/// domain on which [`Runtime::for_each_root_mut`]'s documentation claims **completeness**
/// (`src/runtime/roots.rs`, *Completeness — this walk is whole only at a non-running site*). Its
/// eighteen families are a superset of the value gather's (`root_tests.rs` pins that multiset
/// equality per shared family), so gathering through it covers every store a `stabilize_term`
/// call could have pinned, and a region holding one is **retained by evidence** — the descent sees
/// the word — instead of by a blanket latch.
///
/// The one root family the moving door cannot carry is a raise's live reason
/// ([`RunnerState::for_each_error_live_reason`]): it has a *reader* and no writable slot, which is
/// enough for the liveness question this pass asks. It is added here explicitly.
///
/// # Errors
///
/// A `&'static str` when the enumeration itself fails — today unreachable, because the visitor
/// below cannot fail; a caller must treat it as one more decline (nothing touched).
fn gather_roots(
    runtime: &mut Runtime,
    value_roots: &[Eterm],
) -> core::result::Result<Vec<Eterm>, &'static str> {
    let mut roots: Vec<Eterm> = Vec::with_capacity(value_roots.len() + 64);
    roots.extend_from_slice(value_roots);
    runtime
        .for_each_root_mut(&mut |words| {
            roots.extend_from_slice(words);
            Ok(())
        })
        .map_err(|_| "the runtime's root enumeration refused to complete")?;
    runtime
        .runner_state
        .for_each_error_live_reason(&mut |term| roots.push(term));
    Ok(roots)
}

/// One region a pass may reclaim, and the action it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A retired store region created inside the window: droppable whole.
    StoreRetired(usize),
    /// The store's current region above this window's cursor: resettable.
    StoreCurrent,
    /// An arena region at or above the window's mark.
    Arena,
}

/// One address range a pass may reclaim, with the action it feeds.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    /// The target action.
    target: Target,
    /// The half-open byte range: `[lo, hi)`.
    range: (usize, usize),
}

/// The candidate ranges one window offers, in a stable order.
fn live_ranges(runtime: &mut Runtime, cursor: Cursor) -> Vec<Candidate> {
    let width = size_of::<Eterm>();
    let mut candidates: Vec<Candidate> = Vec::new();
    // The store's current region, above this pass's cursor. A replaced region (a growth inside
    // the window) invalidates the cursor's *address*, not the retired chain, so it only costs the
    // current-region half — see `advance_cursor`.
    let base = heap_region_base(&runtime.eval_heap);
    if base == cursor.store_base && runtime.eval_heap.used() > cursor.store_mark {
        candidates.push(Candidate {
            target: Target::StoreCurrent,
            range: (
                base + cursor.store_mark * width,
                base + runtime.eval_heap.used() * width,
            ),
        });
    }
    for index in cursor.store_retired_baseline..runtime.eval_heap_retired.len() {
        let used = runtime.eval_heap_retired[index].used();
        if used == 0 {
            continue;
        }
        let base = heap_region_base(&runtime.eval_heap_retired[index]);
        candidates.push(Candidate {
            target: Target::StoreRetired(index),
            range: (base, base + used * width),
        });
    }
    // The arena: every region at or above the mark, the marked one only above `mark.used()`.
    let mark_base = cursor.arena_mark.base();
    let mark_used_bytes = cursor.arena_mark.used() * width;
    let mut arena: Vec<(usize, usize)> = Vec::new();
    runtime.arena.for_each_heap_region(|words| {
        let base = words.as_mut_ptr() as usize;
        let end = base + words.len() * width;
        if base < mark_base {
            // An older region: entirely below the mark, so this window never wrote it.
            return;
        }
        let lo = if base == mark_base {
            base + mark_used_bytes
        } else {
            base
        };
        if lo < end {
            arena.push((lo, end));
        }
    });
    candidates.extend(arena.into_iter().map(|range| Candidate {
        target: Target::Arena,
        range,
    }));
    candidates
}

/// The store action: drop every whole-dead region, reset the current region's dead window.
fn store_action(runtime: &mut Runtime, live: &[Candidate], hits: &[bool]) {
    // The current region's window, all-or-nothing like the statement window's own rewind.
    let current_clean = live
        .iter()
        .zip(hits.iter())
        .filter(|(candidate, _)| candidate.target == Target::StoreCurrent)
        .all(|(_, hit)| !*hit);
    if current_clean {
        if let Some(cursor) = runtime.region_reclaim.cursor {
            let base = heap_region_base(&runtime.eval_heap);
            if base == cursor.store_base && runtime.eval_heap.used() > cursor.store_mark {
                let words = runtime.eval_heap.used() - cursor.store_mark;
                if runtime.eval_heap.reset_to(cursor.store_mark).is_ok() {
                    let stats = &mut runtime.region_reclaim.stats;
                    stats.store_words_reclaimed =
                        stats.store_words_reclaimed.saturating_add(words as u64);
                } else {
                    // Unreachable: the cursor was checked against the frontier immediately above.
                    // Recorded rather than assumed, and every word stays where it is.
                    runtime
                        .region_reclaim
                        .decline("the store refused to rewind to the window's cursor");
                }
            }
        }
    }
    // The retired regions: a whole-dead one is dropped (its `Heap::drop` frees the carrier), a
    // live one is kept untouched. No word is moved.
    let mut drop_flags = vec![false; runtime.eval_heap_retired.len()];
    for (candidate, hit) in live.iter().zip(hits.iter()) {
        if *hit {
            continue;
        }
        if let Target::StoreRetired(index) = candidate.target {
            if let Some(flag) = drop_flags.get_mut(index) {
                *flag = true;
            }
        }
    }
    let dropped = drop_flags.iter().filter(|drop| **drop).count();
    if dropped == 0 {
        return;
    }
    let mut words = 0u64;
    let mut index = 0usize;
    runtime.eval_heap_retired.retain(|region| {
        let keep = !drop_flags.get(index).copied().unwrap_or(false);
        if !keep {
            words = words.saturating_add(region.used() as u64);
        }
        index += 1;
        keep
    });
    let stats = &mut runtime.region_reclaim.stats;
    stats.store_regions_dropped = stats
        .store_regions_dropped
        .saturating_add(dropped as u64);
    stats.store_words_reclaimed = stats.store_words_reclaimed.saturating_add(words);
    // The retired chain's **baseline** is unaffected: only regions at or after it are ever
    // candidates, so everything the window must keep below it is still at the same index.
}

/// Re-anchors the window's store cursor at the current frontier.
///
/// The cursor's `store_mark` means something only in the region it was taken in
/// ([`Runtime::grow_eval_heap`] *replaces* the current region), so a replaced region, or a frontier
/// that moved backwards under it, costs this window its current-region half — the retired chain is
/// unaffected, because its candidates are named by index and not by address. Advancing is not a
/// decline: nothing was refused, the window simply has no current-region prefix left to prove.
fn advance_cursor(runtime: &mut Runtime) {
    let Some(cursor) = &mut runtime.region_reclaim.cursor else {
        return;
    };
    let base = heap_region_base(&runtime.eval_heap);
    let used = runtime.eval_heap.used();
    if base != cursor.store_base || used < cursor.store_mark {
        cursor.store_base = base;
        cursor.store_mark = used;
    }
}

/// The runtime's **region-reclaim accounting**.
impl Runtime {
    /// The published region-reclaim counters ([`RegionReclaimStats`]).
    pub fn region_reclaim_stats(&self) -> RegionReclaimStats {
        self.region_reclaim.stats()
    }

    /// The three region-reclaim action bits currently in force ([`RegionReclaimConfig`]) — what
    /// [`Runtime::new`] latched from `BEAM_RT_REGION_RECLAIM`, or what
    /// [`Runtime::set_region_reclaim_enabled`] last set.
    ///
    /// A measurement reads this rather than re-parsing the environment, because the **latched**
    /// configuration is the one the pass acts on: a pin that states a post-reclaim *shape* (an
    /// emptied arena) is only truthful while the action that produces it is enabled, so the pin has
    /// to ask the runtime which state it is in instead of assuming the default.
    pub fn region_reclaim_enabled(&self) -> RegionReclaimConfig {
        self.region_reclaim.config()
    }

    /// Sets the three region-reclaim action bits ([`RegionReclaimConfig`]).
    ///
    /// All are **on** in every production route; this exists so a measurement can hold one action
    /// out of the picture and compare the same workload against itself on the same binary, like
    /// [`Runtime::set_eval_scratch_enabled`]. It can only *disable* an action — every soundness
    /// guard is independent of it, so the words an action would have reclaimed simply stay where
    /// they were.
    pub fn set_region_reclaim_enabled(&mut self, config: RegionReclaimConfig) {
        self.region_reclaim.set_config(config);
    }
}

#[cfg(test)]
mod tests;
