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

//! The crate's own heap-admission point honours the collector's **collection debt**.
//!
//! A no-collect, no-relocation nursery growth ([`Process::grow_nursery`]) retires the space it
//! replaces instead of freeing it, so the nursery becomes a *chain* whose spent capacity only a
//! collection reclaims ([`Process::collection_debt`]). A host takes that route whenever a
//! relocating collection is declined for a receiver whose live roots are not fully published, so a
//! process can keep being grown while its **current** space still has room — the occupancy test
//! ([`Process::should_collect`]) never fires, the chain never collapses, and the footprint climbs
//! instead of levelling off.
//!
//! [`Process::admission_requires_collection`] is the crate's own answer: the occupancy test **OR**
//! the debt. [`Process::collect_if_needed`] runs it, so one debt can never collect twice (a
//! collection collapses the chain — the debt half is self-limiting) and a run of "grow then admit"
//! rounds keeps the young capacity bounded instead of accumulating a retired space per round.

use beam_common::Result;
use beam_process::prelude::*;

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// "Grow then admit" rounds the bounded-capacity case runs.
const ROUNDS: usize = 8;

/// The young nursery's current capacity in words.
fn young_capacity(process: &Process) -> usize {
    process.collector().young().capacity_words()
}

/// **A fresh process carries no debt, so the admission is the occupancy test alone.**
///
/// Nothing has been grown, so there is no retired space to reclaim: the admission answers "no
/// collection" for a request the current space can satisfy, and `collect_if_needed` runs nothing
/// (`minor_gcs` unchanged) — the pre-existing behaviour for a debt-free process.
#[test]
fn a_fresh_process_carries_no_debt_and_admits_without_collecting() -> Result<()> {
    let mut process = Process::new(pid(1))?;
    assert!(!process.collection_debt(), "a single nursery space is no debt");
    assert!(
        !process.should_collect(1),
        "the fresh space has room for the request"
    );
    assert!(
        !process.admission_requires_collection(1),
        "no occupancy pressure and no debt: the admission requires no collection"
    );
    assert!(
        !process.collect_if_needed(1)?,
        "no collection ran for a debt-free admission"
    );
    assert_eq!(process.collector().minor_gcs(), 0, "no collection ran");
    Ok(())
}

/// **A growth sets the debt; the admission collects once and the debt half is self-limiting.**
///
/// The no-collect growth retires the space it replaces, so the debt is set while the current space
/// still has plenty of room — exactly the shape the occupancy test alone cannot see. The admission
/// therefore requires a collection, `collect_if_needed` runs it (collapsing the chain), and an
/// immediately following admission answers "no collection" without touching `minor_gcs` again.
#[test]
fn debt_makes_the_admission_collect_and_cannot_collect_twice() -> Result<()> {
    let mut process = Process::new(pid(2))?;
    let grown = process.grow_nursery(4096)?;
    assert!(grown >= 4096, "the growth covers the request: {grown} words");
    assert!(
        process.collection_debt(),
        "the growth retires the space it replaces: that retention is the debt"
    );
    assert_eq!(
        process.collector().young_space_count(),
        2,
        "the nursery is a chain of two spaces (one retired)"
    );
    assert!(
        !process.should_collect(1),
        "the CURRENT space still has room, so the occupancy test alone would not collect"
    );

    // The debt half is what makes the admission fire — the regression this test pins.
    assert!(
        process.admission_requires_collection(1),
        "a retained chain needs a collection even with a roomy current space"
    );
    assert!(
        process.collect_if_needed(1)?,
        "a collection ran for the debt"
    );
    assert_eq!(process.collector().minor_gcs(), 1, "exactly one collection");
    assert!(
        !process.collection_debt(),
        "the collection collapsed the chain, settling the debt"
    );
    assert_eq!(
        process.collector().young_space_count(),
        1,
        "one space again after the collapse"
    );

    // Self-limiting: the same admission right after the collecting one asks for nothing, because
    // the debt it would have collected for is gone.
    assert!(
        !process.admission_requires_collection(1),
        "the settled debt cannot require a second collection"
    );
    assert!(
        !process.collect_if_needed(1)?,
        "no second collection for the same debt"
    );
    assert_eq!(
        process.collector().minor_gcs(),
        1,
        "the second admission did not collect"
    );
    Ok(())
}

/// **A run of "grow then admit" rounds keeps the young capacity bounded.**
///
/// Each round asks for twice the current capacity — a request the current space cannot satisfy, so
/// the growth really retires a space — and then admits one word. Because the admission honours the
/// debt, every round ends with the chain collapsed to a single space and the capacity back at what
/// the policy sizes from the (empty) live set, so it settles on one small curated size (90 words in
/// the measured run) instead of climbing.
///
/// The bound is contrasted against the **old occupancy-only** premise, measured on a second process
/// that runs the same growth rounds but *does not* collect: there `should_collect(1)` is `false`
/// every round (the current space always has room), so nothing collapses the chain, every round
/// adds a retired space and the capacity grows roughly geometrically (514 838 words after the same
/// eight rounds in the measured run).
#[test]
fn growing_and_admitting_in_rounds_keeps_the_young_capacity_bounded() -> Result<()> {
    // The debt-honouring process.
    let mut process = Process::new(pid(3))?;
    let mut capacities = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        let need = young_capacity(&process) * 2;
        process.grow_nursery(need)?;
        assert!(
            process.collection_debt(),
            "round {round}: the growth retires a space"
        );
        assert!(
            !process.should_collect(1),
            "round {round}: the current space still has room, so occupancy alone sees nothing"
        );
        assert!(
            process.admission_requires_collection(1),
            "round {round}: the debt makes the admission require a collection"
        );
        assert!(
            process.collect_if_needed(1)?,
            "round {round}: a collection ran"
        );
        assert!(
            !process.collection_debt(),
            "round {round}: the collection settled the debt"
        );
        assert_eq!(
            process.collector().young_space_count(),
            1,
            "round {round}: the chain is collapsed back to one space"
        );
        // … and the debt half cannot fire twice for the same growth.
        let minors = process.collector().minor_gcs();
        assert!(
            !process.collect_if_needed(1)?,
            "round {round}: the settled debt requires no second collection"
        );
        assert_eq!(
            process.collector().minor_gcs(),
            minors,
            "round {round}: the second admission ran nothing"
        );
        capacities.push(young_capacity(&process));
    }
    assert_eq!(
        process.collector().minor_gcs(),
        ROUNDS as u32,
        "one collection per round, and none besides"
    );

    // Bounded: the capacity after every round stays within a small constant multiple of the first
    // round's, instead of accumulating a retired space per round.
    let first = capacities[0];
    let peak = *capacities.iter().max().expect("ROUNDS > 0");
    assert!(
        peak <= 4 * first,
        "the honouring process's young capacity must stay bounded (first {first} words, peak \
         {peak} words, per-round {capacities:?}) — without the debt half every round adds another \
         retired space and the capacity climbs"
    );
    assert_eq!(
        capacities[ROUNDS - 1], capacities[ROUNDS - 3],
        "the last rounds settle on the same capacity, so the chain really is collapsing \
         (per-round {capacities:?})"
    );

    // The control: the very same growth rounds under the OLD, occupancy-only premise (`grow_nursery`
    // only — the old admission answered "no collection" because `should_collect(1)` is false).
    let mut legacy = Process::new(pid(4))?;
    for _ in 0..ROUNDS {
        let need = young_capacity(&legacy) * 2;
        legacy.grow_nursery(need)?;
        assert!(
            !legacy.should_collect(1),
            "the legacy premise never sees a need to collect"
        );
    }
    let legacy_capacity = young_capacity(&legacy);
    assert_eq!(
        legacy.collector().young_space_count(),
        ROUNDS + 1,
        "the legacy premise retains one retired space per round"
    );
    assert!(
        legacy_capacity > 100 * peak,
        "the regression this fix removes: the occupancy-only premise grows geometrically \
         ({legacy_capacity} words over {ROUNDS} rounds) while honouring the debt keeps the peak at \
         {peak} words"
    );
    Ok(())
}

/// **A relocation-exempt process keeps its debt.**
///
/// The exemption is the host's assertion that its live terms may sit outside the crate root set, so
/// `collect_if_needed` must stay a no-op (`Ok(false)`, "no collection ran") — even when the debt
/// would otherwise require one. The debt is left in place for whichever non-exempt admission point
/// collects the process later; the exemption is never weakened to satisfy the debt.
#[test]
fn a_relocation_exempt_process_with_debt_keeps_it_and_answers_no_collection() -> Result<()> {
    let mut process = Process::new(pid(5))?;
    // `grow_nursery` is not gated by the exemption (it relocates nothing), so the debt can be
    // acquired while exempt — which is exactly the host's declined-relocation case.
    process.grow_nursery(4096)?;
    assert!(process.collection_debt(), "the growth retires a space");
    process.set_relocation_exempt(true);
    assert!(process.is_relocation_exempt());
    assert!(
        !process.should_collect(1),
        "the current space still has room"
    );
    assert!(
        process.admission_requires_collection(1),
        "the debt would require a collection for a non-exempt process"
    );
    assert!(
        !process.collect_if_needed(1)?,
        "an exempt process never collects here, even with debt"
    );
    assert_eq!(process.collector().minor_gcs(), 0, "no collection ran");
    assert!(
        process.collection_debt(),
        "the debt was NOT settled (no collection ran to collapse the chain)"
    );
    assert_eq!(
        process.collector().young_space_count(),
        2,
        "the retained chain is untouched"
    );
    Ok(())
}
