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

//! The **explicit capacity-give-back** entry point: [`Collector::trim_retired`].
//!
//! A nursery is a chain of spaces, and the only thing that frees a space a
//! [growth](Collector::grow_nursery) retired is a collection. A collection *does* collapse
//! the chain, but it sizes the single surviving space through the policy's **keep band**
//! ([`HeapPolicy::young_after_collection`] / [`HeapPolicy::young_after_fullsweep`]), which
//! deliberately retains a capacity near the target so a live set that swings within one
//! operation does not oscillate the nursery. That band is a good default and a poor answer to an
//! explicit request: a caller that knows the live set has settled far below the chain wants the
//! words back *now*.
//!
//! This module is that request, and nothing else: it runs the ordinary collection (which is what
//! makes the retired spaces' words provably dead) and then installs the live set's target
//! **unconditionally** through the nursery's own trim seam (`trim_after_minor` /
//! `trim_after_fullsweep`). No sizing rule, threshold or default changes: the policy still
//! computes the same figures, and every other collection keeps taking the banded decision it
//! always took.
//!
//! The collection driver itself stays in [`crate::collector`]; this file holds only the
//! give-back decision, so the driver's file keeps its size and its single subject.

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::collector::{Collection, CollectionKind, Collector};

impl Collector {
    /// Gives the nursery's retained capacity back: collects, then installs the live set's target
    /// **unconditionally**, and reports the collection that ran.
    ///
    /// # What the caller must provide
    ///
    /// A **complete root set at a safepoint** — exactly the claim
    /// [`collect`](Collector::collect) carries. Every `Eterm` in `roots` may be **rewritten** by
    /// the collection (it is a root slice, forwarded in place), and any live term the slice
    /// omits is left dangling when the from-spaces are freed. This method cannot verify the
    /// claim and does not try: the root-set contract is the caller's.
    ///
    /// # What it guarantees
    ///
    /// On success the returned [`Collection::kind`] says which collection ran, and afterwards
    ///
    /// * [`young_space_count`](Collector::young_space_count) is `1` — the retired chain is gone;
    /// * [`collection_debt`](Collector::collection_debt) is `false`;
    /// * [`heap_size_words`](crate::HeapObservable::heap_size_words) is exactly the live set's
    ///   target for that kind: [`HeapPolicy::young_minor_target_words`] of the round's copy
    ///   volume after a minor, [`HeapPolicy::young_target_words`] after a fullsweep.
    ///
    /// All three are read off the [`Collection::stats`] the collection reported, so the figure is
    /// the round's own measurement and not a re-derivation.
    ///
    /// # Why it is sound
    ///
    /// A retired space holds no live word **only after a collection evacuated it**, and this
    /// method always collects first: the install that follows therefore replaces a nursery whose
    /// live words are already in the mature space, so no term is ever moved or stranded outside a
    /// collection. The install itself is the collection's own collapse body (a fresh space at the
    /// target capacity, or a rewind when the capacity already equals it).
    ///
    /// # What it does not do
    ///
    /// It does not resize the **mature** space — a fullsweep is the only collection that sizes
    /// that generation, and whether this call runs one is exactly what
    /// [`Collection::kind`] reports (it is chosen by the policy, unchanged). A caller that needs
    /// the mature capacity given back must run a
    /// [fullsweep](Collector::collect_major).
    ///
    /// # Errors
    ///
    /// The collection's own [`BeamError`](beam_common::BeamError): a typed heap exhaustion when
    /// the live set does not fit (nothing copied in that case), an allocation failure, or the
    /// target space's allocation failure. A failed collection leaves the chain as the collection
    /// left it — the trim never runs on an uncollected nursery.
    pub fn trim_retired(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let collection = self.collect(roots)?;
        let policy = self.policy().clone();
        let live = collection.stats.words_copied;
        match collection.kind {
            CollectionKind::Minor => self.nursery_mut().trim_after_minor(&policy, live)?,
            CollectionKind::Major => self.nursery_mut().trim_after_fullsweep(&policy, live)?,
        }
        Ok(collection)
    }
}
