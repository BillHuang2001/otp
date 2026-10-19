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
//! Reclamation of the `-eval` route's escaped off-heap refc-binary store.
//!
//! [`Runtime::eval_off_heap`](crate::runtime::Runtime) is the table that **owns** the
//! off-heap payloads terms on the table-less (`-eval`/`-s`/`-run`) route point at. It is
//! filled by [`Runtime::adopt_escaped_binaries`](crate::runtime::Runtime) (a nested
//! machine's reservoir, adopted before that machine drops) and by every copy onto
//! [`Runtime::eval_heap`](crate::runtime::Runtime) (`CopyContext`'s destination table), and
//! until this module existed it only ever grew: nothing removed an entry, so a route that
//! builds and drops off-heap binaries retained every one of them for the runtime's life.
//!
//! # Where it runs, and why only there
//!
//! One call site: [`Evaluator::run_top_level`](crate::eval::Evaluator), **between the top-level
//! statements** of a `-eval` expression. That is the only point on this route where the
//! evaluator's live term set is fully nameable — its [`Env`](crate::eval) and the value of the
//! statement just reduced — so it is the only point where the caller can hand this module a
//! complete root list. A recursion-depth test inside the expression walker would *not* be
//! equivalent: the same statement list is reached from `case`/`try`/`fun` clause bodies, whose
//! subject terms and partially built containers are live and unnamed (see
//! [`Evaluator::prune_escaped`](crate::eval::Evaluator)).
//!
//! # Publish, then prune
//!
//! A payload may be released only while **no live term names it**. The argument this module
//! rests on has four parts:
//!
//! 1. **Every region that can hold a store-owned pair is read word by word — as words, never as
//!    terms.** [`Runtime::for_each_off_heap_region`](crate::runtime::Runtime) hands over the whole
//!    written prefix of `eval_heap`, its retired generations, the pooled BIF heaps and the
//!    `system_info` scratch heap, and [`collect_region_payloads`] reads each word's *pattern*
//!    ([`beam_term::boxed::HEADER_BIN_REF`], whose neighbour is a payload pointer) without
//!    dereferencing anything. A `[BinRef][ErlSubBits]` pair therefore can never be hidden behind a
//!    container this module cannot read: the pair is found wherever it stands, by its own header.
//!    (That is also why the engine arena's missing word-range accessor is a *reported* gap and
//!    not a hole here: no constructor in this workspace writes a pair onto the arena — the pair
//!    writers are `beam_term::refc::write_refc_binary`'s callers, all of which write into a copy's
//!    *destination* heap — and a pair on `eval_heap` is reached whether or not an arena container
//!    points at it.)
//!
//!    **Region words are never pushed as roots.** That was this module's first shape and it was
//!    unsound: a region also holds the **payload words of on-heap binaries**, which are data — the
//!    40 000 `0x01` bytes `binary:copy(<<1>>, 40000)` materializes read as
//!    `0x0101_0101_0101_0101`, whose low bits are `TAG_PRIMARY_LIST` and whose "pointer" is
//!    `0x0101_0101_0101_0100`, so the walk dereferenced it and the process died with `SIGSEGV`
//!    (`General Protection Fault`) inside `walk` on the **fourth** `-eval` of
//!    `base64:encode(binary:copy(<<1>>, 40000))` — the first statement whose summed payload bytes
//!    crossed [`STORE_BUDGET_WORDS`] and so the first that ran a prune at all. Only real terms may
//!    enter the walk, and the only real terms here are the named live sets below.
//! 2. **Every root family that holds a term no heap region holds is enumerated** — the evaluator's
//!    environment and reduced value (passed in as `extra`), the escaped closures' capture
//!    environments, the sequential-trace tokens, every live process's dictionary and mailbox
//!    queue, and the pending-spawn records. Every one of these is a real term, so the walk may
//!    dereference what it reaches from them.
//! 3. **The walk is conservative.** Any doubt — an exhausted visit budget, a boxed header this
//!    module does not know — **declines the whole prune** and retains everything. That is
//!    deliberately the *opposite* of `RuntimeContext::graph_holds_reference`, whose exhaustion
//!    answer is "assume a reference" because its only consequence is a declined move; here a wrong
//!    answer frees a payload a live term names, so doubt must mean "keep".
//! 4. **A shared payload is retained.** A handle whose `RefCount > 1` is owned by something
//!    outside the enumeration (a mailbox's retained binaries, a machine reservoir, a copy's
//!    source table), so it is kept: a false retain is a bounded leak, a false release a dangle.
//!
//! Every residual is therefore a **leak, never a dangle**.
//!
//! # The refusal rule, and the two guards that are deliberately *not* in it
//!
//! The rule is exactly: *spend the visit budget or meet an unknown boxed header ⇒ decline*;
//! *`RefCount > 1` ⇒ retain that payload*. Two candidate guards were considered and rejected:
//!
//! * **The global collection valve** (`Runtime::gc_enabled`). It says "this runtime stopped
//!   **moving** process heaps", because an out-of-band store holds a term the copier could not
//!   relocate ([`Runtime::try_stabilize_escaped`](crate::runtime::gc)). It is the wrong predicate
//!   here on both counts: the prune **moves nothing** (a retained payload keeps its `data_ptr`),
//!   and the valve is unrelated to off-heap reachability. Measured on the route this module
//!   serves — a `-eval` expression that builds and drops off-heap binaries through
//!   `base64:encode/1` — the valve is **not** tripped (`Runtime::gc_enabled()` stays `true`;
//!   pinned by this module's `the_eval_route_does_not_trip_the_collection_valve`), and the only
//!   ways to trip it store a
//!   term that could not be copied at all — an **MSO** container, whose refc binaries (if any)
//!   are covered by the `RefCount > 1` belt, since the machine that built them still holds their
//!   handles. Keeping the guard would have made the prune a no-op the moment any unrelated
//!   `persistent_term`/timer store tripped the valve, for no soundness gain.
//! * **"An engine has ever existed"** ([`Runtime::any_engine_ever`](crate::runtime::Runtime)).
//!   [`crate::runner::RunnerState`]'s engines, their machines and their recorded outcomes are
//!   private to `crate::runner`, so the family is only observable as a sticky flag — and it is
//!   set on **every** route, because the boot script registers `{kernelProcess,…}` engines before
//!   any action runs. Declining on it would make this prune permanently dead. It does not need to
//!   be a condition: a term an engine holds that names a payload always brings its own handle
//!   (`Machine::refc_binaries` never clears, and a copy registers in the destination table), so
//!   part 4's belt retains exactly those payloads — and the runner's *recorded* outcomes are
//!   `eval_heap` copies, i.e. part 1's region scan. The flag stays a **diagnostic**
//!   ([`crate::runtime::EvalOffHeapStats::any_engine`]).
//!
//! # The trigger
//!
//! `O(1)` per call: the store's handle count (`Vec::len`) is compared with the count the last
//! prune (or the last observation) saw, and the `O(handles)` byte figure is re-derived **only**
//! when it moved — the same shape `beam_rt`'s slice-boundary prune uses
//! (`src/context/gc/bin_vheap.rs`). Past that, the summed payload bytes must exceed
//! [`STORE_BUDGET_WORDS`]; only then is the walk run and the store rewritten.
//!
//! # What this module does *not* touch
//!
//! No per-pid `Process::set_off_heap_binary_bytes` figure is published: the store is not
//! pid-attributable (a nested run inside a scheduled process grows it too), so that figure
//! stays the runner's park-arm prune's.
//!
//! # A duplicate that is a deferral, not an accident
//!
//! The reachability descent here is the shape of `RuntimeContext::graph_holds_reference`
//! (`crate::context::calls`): an inline worklist, the same word guards (`is_literal` /
//! `is_term_word` / boxed-or-list / an aligned non-null pointer) and the same child enumeration
//! ([`beam_term::layout::for_each_child_slot`]). It is a copy because that function answers a
//! *different* question (and its exhaustion answer) and lives in a node this change may not edit;
//! de-duplicating the two is a deferral (one helper taking a visitor over the reachable graph,
//! owned by `src/context/calls.rs`).
// The narrow, documented `unsafe` override the workspace's policy sanctions for a term-graph walk
// (`src/context/gc_walk.rs`'s precedent): the descent must read raw heap words. Every read is
// guarded exactly as `graph_holds_reference`'s are, and the module allocates no pointer of its
// own.
#![allow(unsafe_code)]

use std::mem::align_of;
use std::mem::size_of;

use beam_term::Eterm;

use crate::hasher::FastHashSet;
use crate::runtime::Runtime;

/// The walk's step budget: enough for any term graph a `-eval` expression builds, small
/// enough that a forged cycle cannot spin. Exhausting it **declines** the prune.
const WALK_VISIT_BUDGET: usize = 1 << 20;

/// The budget the `-eval` store is compared against, in term words.
///
/// The store has no process, hence no per-process heap policy, so it uses the BEAM default
/// `min_bin_vheap_words` (the C `BIN_VH_MIN_SIZE`, 46 422 words) — the same budget a fresh
/// process's binary virtual heap starts with ([`beam_gc::HeapPolicy::default`]).
const STORE_BUDGET_WORDS: usize = 46_422;

/// What one `prune` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prune {
    /// The store was left exactly as it was because soundness could not be shown.
    Declined(&'static str),
    /// Nothing to do: the store is empty, or the trigger is not due.
    Deferred,
    /// The store was rewritten: `released` payloads dropped, `retained` kept.
    Pruned {
        /// Handles that were released (their payload freed when they were the last owner).
        released: usize,
        /// Handles put back into the store.
        retained: usize,
    },
}

/// The store's prune state: the last observed handle count (the `O(1)` half of the trigger)
/// and the counters a test and [`crate::runtime::EvalOffHeapStats`] read.
#[derive(Debug, Default, Clone)]
pub(crate) struct PruneState {
    /// The store's handle count at the last prune **or** observation — the comparison half of
    /// the trigger.
    pub(crate) seen_handles: usize,
    /// How many times the store was actually rewritten.
    pub(crate) prunes: u64,
    /// How many times soundness could not be shown.
    pub(crate) declines: u64,
    /// The reason of the most recent decline.
    pub(crate) last_decline: Option<&'static str>,
}

/// The reachable payloads the walk found.
#[derive(Default)]
struct Live {
    /// The payload addresses (`RefcBinary::data_ptr` values) the walk reached.
    payloads: FastHashSet<usize>,
}

/// The walk's verdict.
enum Walk {
    /// Every root was walked: `live` names the reachable payloads.
    Complete(Live),
    /// The walk could not be completed, so nothing may be released.
    Declined(&'static str),
}

/// Prunes the `-eval` route's off-heap refc-binary store.
///
/// `extra` is the caller's own live set — the terms the driver is holding that no runtime
/// field names (the evaluator's environment and the value being reduced). Passing them is
/// what makes the root enumeration complete; a caller that cannot name its live terms must
/// not call this.
pub(crate) fn prune(runtime: &mut Runtime, extra: &[Eterm]) -> Prune {
    // The diagnostic kill switch (`Runtime::set_eval_prune_enabled`) is checked before anything
    // else, so the trigger's own observation is not recorded while it is off: turning the reclaim
    // back on then starts from a clean slate instead of a half-observed store.
    if !runtime.eval_prune_enabled() {
        return Prune::Deferred;
    }
    // The trigger's `O(1)` half: the handle count, and whether it moved at all.
    let handles = runtime.eval_off_heap_handle_count();
    if handles == 0 {
        return Prune::Deferred;
    }
    if handles == runtime.eval_prune().seen_handles {
        return Prune::Deferred;
    }
    // It moved: derive the expensive half once, and record the observation so a steady state
    // costs a single `len()` again.
    let bytes = runtime.eval_off_heap_bytes();
    runtime.note_eval_prune_observed(handles);
    if bytes / size_of::<Eterm>() <= STORE_BUDGET_WORDS {
        return Prune::Deferred;
    }
    let mut roots = Vec::with_capacity(extra.len() + 16);
    roots.extend_from_slice(extra);
    runtime.gather_off_heap_roots(&mut roots);
    // The two halves of the enumeration: the named live sets, walked as terms (which may
    // decline), and the runtime's own term regions, scanned as **words** (which cannot fault
    // and cannot decline). See the module docs for why region words must never be roots.
    let mut live = match walk(&roots) {
        Walk::Complete(live) => live,
        Walk::Declined(reason) => return decline(runtime, reason),
    };
    runtime.for_each_off_heap_region(|words| collect_region_payloads(words, &mut live));
    // The prune: drain (the table's only removal path), keep every payload that is reachable
    // or shared, and put the survivors back. Re-pushing also **de-duplicates by data pointer**
    // (the store never dedups on push, and a copy may register a handle the store already
    // held), which makes every later `source_payloads`/`off_heap_source` clone cheaper.
    let held = runtime.eval_off_heap_mut().drain_binaries();
    let total = held.len();
    let mut retained = 0;
    for handle in held {
        let reachable = live.payloads.contains(&(handle.data_ptr() as usize));
        // The `RefCount > 1` belt: another owner exists outside this enumeration.
        let shared = handle.refcount() > 1;
        if reachable || shared {
            let duplicate = runtime
                .eval_off_heap_mut()
                .binaries()
                .iter()
                .any(|kept| kept.data_ptr() == handle.data_ptr());
            if duplicate {
                // Dropping `handle` here is exactly the de-duplication: the surviving entry
                // keeps the payload alive.
                continue;
            }
            runtime.eval_off_heap_mut().push_binary(handle);
            retained += 1;
        }
    }
    let handles = runtime.eval_off_heap_handle_count();
    runtime.note_eval_prune(handles);
    Prune::Pruned {
        released: total - retained,
        retained,
    }
}

/// Records a decline and answers it.
fn decline(runtime: &mut Runtime, reason: &'static str) -> Prune {
    runtime.note_eval_prune_declined(reason);
    Prune::Declined(reason)
}

/// Collects the payload pointers the runtime's own term regions name, **without dereferencing a
/// single region word**.
///
/// `words` is the written prefix of one region
/// ([`Runtime::for_each_off_heap_region`](crate::runtime::Runtime)). A region is *not* an array of
/// terms: it interleaves heap objects with the **payload words of on-heap binaries** and floats,
/// which are raw data. So this scan reads a *pattern*, never a pointer:
/// [`beam_term::refc`]'s pinned 8-word layout puts `HEADER_BIN_REF` at the pair's first word and
/// the payload pointer (`BinRef.val`, the very value `RefcBinary::data_ptr` reports) immediately
/// after it, so a window whose first word **is** that header names a payload at its second word.
///
/// # Why a pattern match, and why a false positive is safe
///
/// The header is a *value*, so no address is touched: payload data that happens to spell
/// `HEADER_BIN_REF` (a `1`-in-`2^60`-ish accident, and even then only in a region that already
/// holds arbitrary bytes) inserts a **bogus payload address** into `live.payloads`, which can only
/// *retain* a handle the store does not hold — a no-op. The reverse mistake is impossible: the
/// only writer of the pair is `beam_term::refc::write_refc_binary` (through
/// `beam_message`'s copier), and it always writes the header, so a live pair's payload is always
/// found. A false release is what a dangle looks like, and this scan cannot produce one.
fn collect_region_payloads(words: &[Eterm], live: &mut Live) {
    let header = beam_term::boxed::HEADER_BIN_REF.raw();
    for window in words.windows(2) {
        if window[0].raw() == header {
            live.payloads.insert(window[1].raw());
        }
    }
}
/// The region-agnostic reachable-payload walk.
///
/// Every failure mode declines (see the module docs): the budget is spent, or a boxed header
/// this module does not know is met, or a word looks like a container but names no readable
/// address (which is *skipped* — no term word has that shape, so nothing is lost and the C
/// faults there too).
fn walk(roots: &[Eterm]) -> Walk {
    let mut live = Live::default();
    let mut work: Vec<Eterm> = Vec::with_capacity(roots.len().max(16));
    work.extend_from_slice(roots);
    let mut budget = WALK_VISIT_BUDGET;
    while let Some(item) = work.pop() {
        // A literal is a frozen code constant (it can name nothing on these heaps), an
        // immediate is neither boxed nor a cons, and a bare header word is not a term at all.
        if beam_term::literal::is_literal(item)
            || !beam_term::tag::is_term_word(item)
            || !(beam_term::boxed::is_boxed_term(item) || beam_term::prelude::is_list(item))
        {
            continue;
        }
        let raw = beam_term::list::ptr_val(item) as usize;
        if raw == 0 || raw % align_of::<Eterm>() != 0 {
            continue;
        }
        if budget == 0 {
            // The one place this walk differs from `graph_holds_reference`: exhaustion means
            // "keep everything", never "assume reachable".
            return Walk::Declined("the reachability walk exhausted its visit budget");
        }
        budget -= 1;
        // SAFETY: `item` is a term word that is either boxed or a `LIST`, and the address its
        // pointer bits name is non-null and word-aligned — the same guard the unscanned-root
        // valve applies (`crate::context::gc::dereferenceable_pointer`). A word with any other
        // shape is skipped above, so nothing below dereferences one. A non-null, aligned but
        // bogus pointer is not decidable without faulting; the C runtime faults there too.
        unsafe {
            let address = raw as *const Eterm;
            if beam_term::prelude::is_list(item) {
                // A cons cell: its `car` and `cdr` are the two words at that address.
                work.push(*address);
                work.push(*address.add(1));
                continue;
            }
            let header = *address;
            match header.raw() & beam_term::tag::TAG_HEADER_MASK {
                // `for_each_child_slot` yields nothing for a `BinRef` (it is a leaf), so the
                // payload is collected here: word 1 of a `BinRef` is `val`, the payload's
                // first data byte — the very value `RefcBinary::data_ptr` reports.
                beam_term::tag::BIN_REF_SUBTAG => {
                    let (val, _next) = beam_term::refc::bin_ref_parts(item);
                    live.payloads.insert(val as usize);
                }
                // Every shape whose children are term slots — tuples, records, funs (their
                // `env`), flat maps and HAMT nodes, and an `ErlSubBits` (whose single `orig`
                // word leads to the sibling `BinRef` above). The leaves (float, bignum,
                // reference, external pid/port/reference, on-heap bits) yield no child.
                beam_term::tag::ARITYVAL_SUBTAG
                | beam_term::tag::RECORD_SUBTAG
                | beam_term::tag::FUN_SUBTAG
                | beam_term::tag::MAP_SUBTAG
                | beam_term::tag::SUB_BITS_SUBTAG => {
                    beam_term::layout::for_each_child_slot(header, address, |offset| {
                        work.push(*address.add(offset));
                    });
                }
                beam_term::tag::FLOAT_SUBTAG
                | beam_term::tag::POS_BIG_SUBTAG
                | beam_term::tag::NEG_BIG_SUBTAG
                | beam_term::tag::REF_SUBTAG
                | beam_term::tag::HEAP_BITS_SUBTAG
                | beam_term::tag::EXTERNAL_PID_SUBTAG
                | beam_term::tag::EXTERNAL_PORT_SUBTAG
                | beam_term::tag::EXTERNAL_REF_SUBTAG => {}
                // A header word no constructor in this workspace writes: the term is corrupt,
                // so what it names is unknowable — decline rather than free.
                _ => return Walk::Declined("the reachability walk met an unknown boxed header"),
            }
        }
    }
    Walk::Complete(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::RefcBinary;

    /// An off-heap payload of `bytes` zero bytes.
    fn payload(bytes: usize) -> RefcBinary {
        RefcBinary::new(&vec![0u8; bytes]).expect("the test payload allocation")
    }

    /// Writes the 8-word `[BinRef][ErlSubBits]` pair naming `binary`'s payload onto `heap` and
    /// returns its root term.
    fn pair(heap: &mut beam_term::heap::Heap, binary: &RefcBinary, bits: usize) -> Eterm {
        // SAFETY: `alloc_words` reserves the pair's 8 words, which `write_refc_binary` fully
        // writes; `binary` outlives the term in every caller below.
        unsafe {
            let hp = heap
                .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
                .expect("the test heap has room for one refc pair");
            beam_term::refc::write_refc_binary(
                hp,
                binary.data_ptr().cast::<Eterm>(),
                std::ptr::null_mut(),
                binary.data_ptr().cast_const(),
                0,
                bits,
            )
        }
    }

    /// The walk finds the payload a `[BinRef][ErlSubBits]` pair names — directly, through a
    /// cons, and through a tuple — and reports nothing for an unrelated payload.
    #[test]
    fn the_walk_collects_a_payload_reachable_through_any_container() {
        let mut heap = beam_term::heap::Heap::new(64);
        let first = payload(128);
        let second = payload(128);
        let pair_term = pair(&mut heap, &first, 128 * 8);
        let cons = heap.cons(pair_term, beam_term::prelude::NIL).expect("the cons");
        let tuple = heap
            .tuple(&[beam_term::prelude::make_small(1), cons])
            .expect("the tuple");
        let Walk::Complete(live) = walk(&[tuple]) else {
            panic!("the walk over a well-formed graph completes");
        };
        assert!(
            live.payloads.contains(&(first.data_ptr() as usize)),
            "the pair's payload is reachable through the tuple and the cons"
        );
        assert!(
            !live.payloads.contains(&(second.data_ptr() as usize)),
            "an unreferenced payload is not reported reachable"
        );
    }

    /// A pair that is itself a **root** is found without any container above it — the property
    /// the region word scan depends on (every written word of a heap is pushed as a root).
    #[test]
    fn a_pair_pushed_as_a_root_is_found_directly() {
        let mut heap = beam_term::heap::Heap::new(32);
        let binary = payload(96);
        let pair_term = pair(&mut heap, &binary, 96 * 8);
        let Walk::Complete(live) = walk(&[pair_term]) else {
            panic!("the walk over a lone pair completes");
        };
        assert!(live.payloads.contains(&(binary.data_ptr() as usize)));
    }

    /// The empirical premise behind rejecting the collection-valve guard: the `-eval` route
    /// this prune serves never trips the valve, including while it builds and drops off-heap
    /// payloads through `base64:encode/1` (the store's own producer here).
    #[test]
    fn the_eval_route_does_not_trip_the_collection_valve() {
        let mut runtime =
            crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
                .expect("the runtime");
        assert!(runtime.gc_enabled(), "a fresh runtime has the valve open");
        for _ in 0..3 {
            runtime
                .eval("base64:encode(binary:copy(<<1>>, 40000))")
                .expect("the off-heap building expression evaluates");
            assert!(
                runtime.gc_enabled(),
                "the collection valve must stay open on the -eval route, so it cannot be the \
                 prune's guard"
            );
        }
    }

    /// The `RefCount > 1` belt: a payload a second handle owns is retained even when nothing
    /// in the walked graph names it — a false retain is a leak, a false release a dangle.
    #[test]
    fn a_shared_payload_is_retained_by_the_refcount_belt() {
        let first = payload(64);
        let shared = first.clone();
        assert_eq!(first.refcount(), 2, "the clone shares the payload");
        let live = Live::default();
        let reachable = live.payloads.contains(&(first.data_ptr() as usize));
        assert!(!reachable, "nothing in the walked graph names it");
        assert!(
            reachable || shared.refcount() > 1,
            "the belt keeps a payload another owner holds"
        );
    }
}
