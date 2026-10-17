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

//! The engine's **heap-admission point** (`ExecutionContext::ensure_heap` /
//! `ensure_heap_full`) and the three allocation seams
//! (`alloc_words`/`alloc_words_rooted`/`heap_available`) for a *scheduled* process.
//!
//! A process running as a scheduler slice allocates on **its own**
//! [`beam_process::Process`] young heap and is collected at the heap-admission point, so its
//! peak memory is bounded by its live set instead of growing with the loop count. That point
//! also **grows** the nursery when a request is larger than it
//! ([`Process::ensure_capacity`], the collector's on-demand growth seam) rather than leaving
//! the request to the arena. The unrooted [`alloc_words`] seam — which is handed **no** root
//! set and so may not collect — instead grows the nursery through the **no-relocation**
//! [`Process::grow_nursery`] seam and retries the bump, so a metadata-only allocation loop
//! grows rather than falling back to the arena forever. Everything else keeps the runtime's
//! pointer-stable arena, byte for byte as before — see [`crate::runtime::gc`] for the state,
//! the accounting and the safety valves.
//!
//! # The root set is the WHOLE [`RootSet`], bounded to its pointer-bearing prefix
//!
//! The root set a collection relocates is the **executing machine's whole live root set** as
//! the engine hands it over: the X-register file, the **whole** Erlang stack and the FR file.
//! The two in-slice doors ([`ensure_heap_full`] and [`alloc_words_rooted`]) hand the whole root
//! set over — each register file bounded to its pointer-bearing prefix, which is *not* the
//! engine's `live` hint (see below) — while the runner's park arm bounds the FR region to the
//! machine's live leading prefix [`Machine::fr_mark`](beam_engine::Machine::fr_mark)
//! ([`publish_roots_full`]/[`scatter_roots_full`]).
//!
//! # The pointer-bearing prefix — why a register file may be bounded
//!
//! A copying collector acts on a root word only if it is a heap **pointer**: an immediate is
//! never relocated and is never a reachability edge, so it can neither keep a term alive nor
//! add a "reaches an unscanned region" fact. [`root_is_unscanned`] itself answers `false` for
//! any word that is neither boxed nor a list, so **omitting a non-pointer word cannot change
//! the valve's decision** — it can only skip work. Each in-slice door therefore bounds both
//! register files to their **pointer-bearing prefix**: one past the **last** boxed-or-list word
//! of the file (`NIL` and every other immediate is neither), computed **once** per call by
//! [`machine_root_bounds`] and used consistently for the iterator handed to
//! [`any_root_is_unscanned`], for [`publish_roots_full`] and for [`scatter_roots_full`]. The
//! Erlang stack is always published whole, because there are no frame liveness maps.
//!
//! [`pointer_prefix_len`] finds that prefix with a backwards **block sweep**: a
//! [`POINTER_PREFIX_BLOCK`]-word block whose bitwise-ANDed words still carry the `IMMED1` primary
//! tag is wholly immediate and is skipped on one cache-line read, and the block that stops the
//! sweep is the only one resolved word by word — the per-word fallback therefore examines at most
//! one block (or a sub-block tail, for a file whose blocks were all skipped), never the file.
//!
//! The bound keeps every live root by construction: it is "last pointer-bearing index + 1", so
//! a live or boxed word at a high index is always included, and only a tail that is *already*
//! immediate is left out — an FR file a process never wrote (1024 `NIL` slots) publishes
//! nothing from that file. The word order and the length contract survive because the two
//! halves of a hand-off are given the **same** counts; the omitted tail stays byte-identical
//! (neither half writes it, since it is not a root) and the machine's `NIL` FR slots beyond
//! the bound are left untouched exactly as before.
//!
//! [`RootSet::live`] is only a *hint* and is deliberately **ignored**: a copying collector may
//! always over-approximate its root set (it can only keep a term alive that would otherwise
//! die, never free a live one), and the engine's "dead" x registers are not provably dead —
//! a slot the engine declared dead at one admission point can still be read by a later
//! instruction, so leaving it un-relocated lets the heap move out from under it.
//!
//! # Why the gate is a *per-call* latch
//!
//! The root set this module relocates is the **executing machine's** whole [`RootSet`], handed
//! in by [`Machine::ensure_heap`](beam_engine::Machine) / `Machine::alloc_rooted`. A
//! [`RuntimeContext`] is, however, shared by *nested* runs: an external module call
//! ([`RuntimeContext::run_export`]) builds a **new** [`Machine`](beam_engine::Machine) on the
//! same context. A collection driven from that nested machine's admission point would write
//! the *nested* roots into the process's root set and copy the relocated words back into the
//! *nested* machine's slices, leaving the outer machine's terms dangling.
//!
//! The gate is therefore a latch on the context that only the slice's own context wrapper
//! arms ([`BudgetContext::ensure_heap`](crate::runner)/`BudgetContext::alloc_words_rooted`)
//! immediately before delegating here, and that this module **takes**
//! ([`std::mem::take`]) at that one admission point. Every nested path — a new machine in
//! `run_export`, a fun applied by a bridge, the `-eval` evaluator — is a plain
//! [`RuntimeContext`] whose latch is therefore never set, so it can never collect.
//!
//! # The float guard — the narrow door's alone
//!
//! The **narrow** door ([`ensure_heap`]) receives only an x-register prefix and the stack and
//! can never see the machine's FR file, so a collection it drove could dangle a live float
//! register. Its caller-module guard ([`module_may_be_collected`] — the call site's module must
//! be loaded **and** free of any float-family instruction, cached per module name) is therefore
//! load-bearing and is the only thing that keeps that door sound for float code. The engine
//! reaches only the **full** door (`Machine::ensure_heap`/`test_heap`/`allocate_heap` →
//! [`ExecutionContext::ensure_heap_full`](beam_engine::ExecutionContext::ensure_heap_full)), so
//! the narrow form is not on the engine's path today.
//!
//! The **full** doors ([`ensure_heap_full`] and [`alloc_words_rooted`]) hand the machine's whole
//! [`RootSet`] over, the FR file included, each register file bounded only to its
//! pointer-bearing prefix ([`machine_root_bounds`] — and every **live** float register holds a
//! boxed float term, so it always lies inside that bound). A collection over them is therefore
//! FR-safe for **any** module: a live float register is relocated exactly like an x register or
//! a stack word. They apply **no** float guard at all — [`module_uses_floats`] is not
//! consulted there — and a float-using call site collects like any other. What they do keep is
//! the "no caller module" refusal (a fun entry, the `-eval` evaluator): such a run has no
//! machine to name, and is left un-collected rather than guessed at.
//!
//! The **cross-module call seam**
//! ([`RuntimeContext::resolve_external_call`](crate::RuntimeContext)) does not consult this
//! guard at all: a float-using callee is entered in-machine exactly like any other, which is
//! safe because it runs under the caller's machine and that machine's collection goes through
//! the full door. A float register still live when a slice ends is caught out of band by the
//! runner's post-slice [`Machine::fr_mark`](beam_engine::Machine::fr_mark) check, which opens a
//! **per-slice relocation exemption** on the process (`Process::set_relocation_exempt(true)`),
//! cleared by the next slice entry. `Runtime::gc_process_enabled` is still checked **first** by
//! both doors and refuses a pinned, halted or globally-disabled pid outright, but `pinned` is
//! **never** set in production — the exemption is scoped to the inter-slice window, so a
//! float-using loop collects again at its own heap-admission points and its `collections`
//! **scale** with the loop count (`tests.rs::the_post_slice_float_pin_plateaus_a_float_using_loops_collections`).
//! The exemption is triggered by float **operations** that write the FR file (`fconv`/`fmul`), not
//! by a float *literal* in a data structure, which the compiler folds into the literal area and
//! which therefore never writes FR. An unresolvable call site (a fun entry or the `-eval`
//! evaluator, which have no `current_module`) is not collected over either — conservative,
//! never wrong.

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_gc::{AnyCollector, Collection, CollectionKind, ScannedBounds};
use beam_loader::{Instr, Module};
use beam_process::{Process, ProcessTable, MAX_REG};
use beam_term::Eterm;

use crate::runtime::Runtime;
use crate::RuntimeContext;

/// Whether **any** machine root handed to a collection site, or any container in the process's
/// own root set, **reaches** a container in an unscanned region (the building block is
/// [`root_is_unscanned`]).
///
/// This is the **valve** every collection site consults: it is the conservative reachability
/// check of [`crate::context::gc_walk`]. The single-level test it replaces could only see a
/// root's *own* address, so a scanned container holding an unscanned one holding a nursery
/// pointer slipped through; the walk descends through the scanned region and reports that
/// shape. Declining is the only outcome it can produce, and declining only costs a collection —
/// the allocation falls back to the pointer-stable arena — so it can never turn a working
/// program into a failing one.
///
/// It is re-exported here (rather than called by its own path at each site) so the three
/// admission-point call sites and the runner's park arm cannot drift onto different valves.
pub(crate) use super::gc_walk::{any_root_is_unscanned, valve_verdict};

/// Whether `module`'s code contains any **float-family** instruction, i.e. anything that
/// reads or writes the machine's float-register file
/// ([`Machine::fr`](beam_engine::Machine)).
///
/// The list is exactly the `beam_loader` IR's float instructions (`Fmove`/`Fconv`/`Fadd`/
/// `Fsub`/`Fmul`/`Fdiv`/`Fnegate`) — the only writers of the FR file in this engine. The
/// legacy `fclearerror`/`fcheckerror` opcodes are not lowered, so no other instruction can
/// touch it.
pub(crate) fn module_uses_floats(module: &Module) -> bool {
    module.functions.iter().any(|function| {
        function.code.iter().any(|instr| {
            matches!(
                instr,
                Instr::Fmove { .. }
                    | Instr::Fconv { .. }
                    | Instr::Fadd { .. }
                    | Instr::Fsub { .. }
                    | Instr::Fmul { .. }
                    | Instr::Fdiv { .. }
                    | Instr::Fnegate { .. }
            )
        })
    })
}

/// Advances `heap`'s allocation pointer by exactly `n` words and returns the block base.
///
/// # Why this is not `Heap::alloc_words`
///
/// [`beam_alloc::Heap::alloc_words`] is an `unsafe fn` (it hands out *uninitialised* words)
/// and this crate is `#![deny(unsafe_code)]`. [`Heap::tuple`] is the safe equivalent of
/// exactly the same bump: it writes a valid arityval header at the block base, writes
/// `items.len()` element words after it and advances `used` by `items.len() + 1`. So a
/// `tuple` over `n - 1` placeholders reserves `n` words and every word of the block is a
/// well-formed term word ([`NIL`](beam_term::NIL), an immediate the collector never
/// relocates) until the engine overwrites it — which it always does, in full, before the
/// block can be reached from any root.
///
/// `fill` is the caller's reusable placeholder slice (see
/// [`Runtime::gc_scratch`](crate::runtime::gc)); it must not alias `heap` and must hold at
/// least `n - 1` words. A request larger than [`MAX_ARITYVAL`](beam_term::MAX_ARITYVAL) is an
/// honest error — the caller falls back to the pointer-stable arena.
///
/// # Errors
///
/// The heap's own exhaustion error when the block does not fit, and the arity error for an
/// impossible `n`.
pub(crate) fn bump(heap: &mut Heap, n: usize, fill: &[Eterm]) -> Result<*mut Eterm> {
    if n == 0 {
        return Ok(heap.hp());
    }
    let base = heap.hp();
    heap.tuple(&fill[..n - 1])?;
    Ok(base)
}

/// Whether `name`'s module may be collected over: `true` only when the module is loaded
/// **and** contains no float-family instruction.
///
/// An unknown or not-yet-loaded module answers `false`: this is the conservative direction
/// (a wrong `true` could corrupt float code; a wrong `false` only costs a collection), and it
/// is never cached, so a module that is loaded later is answered correctly.
fn module_may_be_collected(runtime: &mut Runtime, name: &str) -> bool {
    if let Some(answer) = runtime.module_float_free_cached(name) {
        return answer;
    }
    match runtime.code_server.module_handle(name) {
        Some(module) => runtime.module_is_float_free(name, &module),
        None => false,
    }
}

/// Whether `ptr` lies inside `heap`'s live storage — `[base, hend)`, i.e. the caller's term
/// words and the not-yet-handout tail of the same block.
///
/// [`Heap::hp`] is the bump pointer and [`Heap::hend`] is one past the block, so the block's
/// base is `hp - used` term words back from `hp`.
fn in_heap_block(heap: &Heap, ptr: usize) -> bool {
    let words = heap.used().saturating_mul(std::mem::size_of::<Eterm>());
    let base = (heap.hp() as usize).wrapping_sub(words);
    let end = heap.hend() as usize;
    ptr >= base && ptr < end
}

/// Which side of the collector's scan boundary a heap pointer lies on — the classification
/// [`root_is_unscanned`] and the reachability walk both build on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WordRegion {
    /// A pointer into a region the collector **relocates**: any space of the young nursery chain
    /// (the current one or a growth-retired one — a collection evacuates the whole chain) or the
    /// mature generation.
    Scanned,
    /// A container in a region the collector leaves **in place**: the pooled BIF heaps
    /// ([`Runtime::take_bif_heap`]), the `-eval` heap ([`Runtime::stabilize_term`]), the
    /// pointer-stable arena ([`alloc_words`]'s fallback), a [`beam_message`] heap fragment.
    Foreign,
}

/// Whether `term` is a **container in a region the collector does not scan whose relocation
/// would leave a word behind** — the precondition that makes a collection of the owning process
/// unsound.
///
/// A copying collector relocates exactly the words it finds in its *from* set (every space of
/// the young nursery chain, plus the mature space for a major) and classifies every other
/// address as [`Target::Foreign`](beam_gc): it leaves such a pointer where it is **and does not
/// follow it**. Its two obligations toward a container are to *follow* the child words it finds
/// there (they keep a young object alive) and to *update* them when it moves one, and a container
/// outside the scanned set admits **neither**. The test is therefore the conjunction of two
/// facts, and both are required:
///
/// 1. the term is a container in a region the collector does not scan — an **immediate** (neither
///    boxed nor a cons) and a **literal** (a frozen code constant in the literal area, which never
///    points into a movable heap) are neither ([`word_region`]); and
/// 2. at least one word it **forwards** is a pointer into a region the collector *does* scan
///    ([`forwards_scanned_word`]).
///
/// # Why the second fact is part of the test
///
/// A word the collector would not have to follow or update is a word it may safely ignore, so a
/// foreign container that forwards only immediates, literals and other foreign pointers dangles
/// nothing and keeps nothing alive: relocating the scanned regions under it is byte-for-byte the
/// collection the graph would have received if the container had not existed. Reading those
/// forwarded words is sound because Erlang terms are **immutable** — a slot that holds an
/// immediate now cannot become a scanned pointer later, so a single read at the safepoint is the
/// whole truth.
///
/// That is what keeps an ordinary program collectable. The runtime itself mints shared, foreign
/// containers: `make_ref/0`'s frozen M1 block
/// (`[make_header(3, REF_SUBTAG), make_small(0), make_small(0), make_small(id)]`, written by
/// [`beam_term::boxed::write_internal_ref`]) and every `RuntimeHooks`-served BIF's container
/// result — `erlang:statistics(garbage_collection)`'s `{Collections, WordsReclaimed, 0}` tuple
/// (the shape `erlang:statistics/1` answers through `beam_bifs`'s `RuntimeHooks` seam, whose
/// result is deliberately *not* re-homed onto the process heap) — are all-immediate blocks on the
/// pooled BIF heap. A process that keeps one live across its loop forwards nothing, so its
/// collections must keep running; a test that declared such a root a hazard would stop collecting
/// for as long as the term lived, which is exactly what a bounded heap cannot afford
/// (`tests/foreign_root_loop.rs`).
///
/// # The transitive case is the walk's
///
/// A foreign container can forward **another** foreign container that forwards a scanned word;
/// that chain is resolved by [`any_root_is_unscanned`], which expands foreign containers to any
/// depth. This single-term form is the depth-1 answer the per-word root classification uses.
///
/// # The scanned test: an aggregate fast reject, then the exact fallback
///
/// "Scanned" is decided in two tiers ([`outside_bounds`]), and the second one exists only to skip
/// work in the first: the current mutator block directly ([`in_heap_block`]), then the collector's
/// aggregate geometry ([`AnyCollector::scanned_bounds`]). Every walk and every admission point
/// reads that aggregate **once** and hands it down as a [`ScannedBounds`] value
/// ([`any_root_is_unscanned`]), so a per-word classification never re-reads it.
pub(crate) fn root_is_unscanned(process: &Process, term: Eterm) -> bool {
    // The aggregate is read **once** per classification and handed down, rather than re-read per
    // forwarded word: `ScannedBounds` is a handful of `Copy` fields derived from the collector's
    // current geometry, so this is **hygiene, not a claimed win** — a `beam_gc` peer measured the
    // whole valve at 0.40 % of samples, and hoisting an aggregate load cannot move that. What it
    // buys is that the per-word classification and the forwarding test can never read two
    // different geometries inside one decision.
    let bounds = process.collector().scanned_bounds();
    root_is_unscanned_with(process, &bounds, term)
}

/// [`root_is_unscanned`] against a [scanned-bounds](ScannedBounds) value the caller already read
/// — the form the reachability walk uses, so one aggregate read serves a whole walk.
pub(crate) fn root_is_unscanned_with(
    process: &Process,
    bounds: &ScannedBounds,
    term: Eterm,
) -> bool {
    match word_region(process, bounds, term) {
        Some(WordRegion::Foreign) => forwards_scanned_word(process, bounds, term),
        Some(WordRegion::Scanned) | None => false,
    }
}

/// Which region `word` points into: `None` for a word no relocation touches (an immediate, a
/// literal-area pointer), otherwise the side of the scan boundary the container lives on.
///
/// This is the **one** place the scanned test is applied, so the per-word root classification,
/// the walk's descent and the foreign-container expansion can never disagree about a word.
pub(crate) fn word_region(
    process: &Process,
    bounds: &ScannedBounds,
    word: Eterm,
) -> Option<WordRegion> {
    if !is_pointer_bearing(word) {
        return None;
    }
    if beam_term::literal::is_literal(word) {
        return None;
    }
    let ptr = beam_term::list::ptr_val(word) as usize;
    if in_heap_block(process.heap(), ptr) || !outside_bounds(process.collector(), bounds, ptr) {
        Some(WordRegion::Scanned)
    } else {
        Some(WordRegion::Foreign)
    }
}

/// Whether `word` is a heap pointer into a region the collector **relocates** — one of the words
/// a collection must follow and update, and therefore the word that makes the foreign container
/// holding it a hazard.
pub(crate) fn word_is_scanned(process: &Process, bounds: &ScannedBounds, word: Eterm) -> bool {
    matches!(word_region(process, bounds, word), Some(WordRegion::Scanned))
}

/// Whether `ptr` lies outside **every** region `collector` scans — the two-tier scanned test
/// behind [`word_region`], for an address already known to be outside the process's current
/// mutator block.
///
/// Tier one is the collector's aggregate geometry, the `bounds` the caller read once:
/// [`beam_gc::ScannedBounds::definitely_unscanned`] answers for **every** address outside it — the
/// pooled BIF heaps, the `-eval` heap, the pointer-stable arena and a message heap fragment,
/// which is what a foreign container really is — with two compares. That is exact, because the
/// aggregate is a superset of every scanned region: a reject would have been a reject through the
/// exact per-region tests too.
///
/// Tier two is those exact tests ([`AnyCollector::young_contains`] — the whole nursery chain,
/// current space and growth-retired spaces, `O(log #spaces)` — then
/// [`AnyCollector::mature_contains`]), run only when the aggregate **cannot** decide: an address
/// in a hole between two scanned regions, in a space's reserved-but-unallocated tail, or on a
/// free line of a mark-region mature generation. `may_contain == true` is never treated as
/// containment (see [`beam_gc::ScannedBounds`] for why a fast accept would be unsound); it only
/// means "ask them".
fn outside_bounds(collector: &AnyCollector, bounds: &ScannedBounds, ptr: usize) -> bool {
    if bounds.definitely_unscanned(ptr as *const Eterm) {
        return true;
    }
    outside_scanned_exactly(collector, ptr)
}

/// **Tier two on its own**: whether `ptr` lies outside every region `collector` relocates,
/// decided by the exact per-region tests with no aggregate in front of them.
///
/// [`outside_bounds`] is this behind the aggregate fast reject, so the two agree by construction;
/// the filter-less form is exposed for the `valve_bounds` suite, which asserts that the reject
/// never changes a decision.
pub(crate) fn outside_scanned_exactly(collector: &AnyCollector, ptr: usize) -> bool {
    !collector.young_contains(ptr as *const Eterm)
        && !collector.mature_contains(ptr as *const Eterm)
}

/// [`word_region`] against the exact scanned test alone — the classifier with the aggregate fast
/// reject removed, for the same equivalence oracle.
///
/// Test-only: the production path always reaches the region classification through
/// [`word_region`] (which is what the aggregate fast reject exists for), so the filter-less form
/// exists purely as the `valve_bounds` suite's oracle.
#[cfg(test)]
pub(crate) fn word_region_exactly(process: &Process, word: Eterm) -> Option<WordRegion> {
    if !is_pointer_bearing(word) || beam_term::literal::is_literal(word) {
        return None;
    }
    let ptr = beam_term::list::ptr_val(word) as usize;
    if in_heap_block(process.heap(), ptr) || !outside_scanned_exactly(process.collector(), ptr) {
        Some(WordRegion::Scanned)
    } else {
        Some(WordRegion::Foreign)
    }
}

/// Whether the container `term` names forwards at least one word a relocation would have to
/// follow — a cons cell's two halves, or the canonical child slots of a boxed object.
///
/// [`beam_term::layout::for_each_child_slot`] is the enumeration both `beam_message`'s copier and
/// the GC reachability walk drive, so the valve cannot disagree with the collector about which
/// words an object forwards — notably `SUB_BITS_SUBTAG`, whose `orig` word *is* forwarded. The
/// values of those words are then classified by [`word_is_scanned`]: a leaf forwards nothing, a
/// shape whose cells are all immediates forwards nothing, and each one is exactly as safe in a
/// foreign region as an immediate is.
pub(crate) fn forwards_scanned_word(
    process: &Process,
    bounds: &ScannedBounds,
    term: Eterm,
) -> bool {
    forwards_word_matching(term, |word| word_is_scanned(process, bounds, word))
}

/// [`forwards_scanned_word`] against an arbitrary child-word predicate — the enumeration without
/// the classifier, so a caller that needs a *different* scanned test (the `valve_bounds` suite's
/// aggregate-free oracle) cannot drift from the shapes the valve walks.
///
/// The predicate is called for a child word only until one of them answers `true`, so a shape
/// with many slots pays at most one classification per slot.
///
/// # Safety
///
/// The caller must pass a **live** boxed-or-cons term — one reachable from the root set or from
/// another live object — so the words it names are readable, aligned term words. The object may
/// live in **any** region, scanned or not: the reads only touch words belonging to the object.
#[allow(unsafe_code)]
pub(crate) fn forwards_word_matching<F>(term: Eterm, mut matches: F) -> bool
where
    F: FnMut(Eterm) -> bool,
{
    if beam_term::prelude::is_list(term) {
        // SAFETY: `term` is a live cons cell (the caller's contract). A cons is header-less, so
        // the two words at its address are its `car` and `cdr` — both live term words.
        let address = beam_term::list::ptr_val(term) as *const Eterm;
        // SAFETY: as above; both word reads are inside the live cons cell.
        return unsafe { matches(*address) || matches(*address.add(1)) };
    }
    // SAFETY: `term` is a live boxed term (the caller's contract), so its header word is a
    // readable, aligned term word at the address the term's pointer bits name.
    unsafe {
        let address = beam_term::boxed::boxed_val(term) as *const Eterm;
        let header = *address;
        let mut forwards = false;
        // SAFETY: `address` heads the live object `header` names, so `for_each_child_slot` visits
        // exactly that shape's term-reference slots — offsets relative to the header word, every
        // one in bounds and live. The closure stops classifying once it has its answer, so a shape
        // with many slots pays at most one classification per slot and no more.
        beam_term::layout::for_each_child_slot(header, address, |offset| {
            if !forwards {
                forwards = matches(*address.add(offset));
            }
        });
        forwards
    }
}

/// The width of the blocks [`pointer_prefix_len`]'s fast path skips at a time: one 64-byte cache
/// line of the backwards sweep, the cheapest unit for the run of `NIL` slots a 1024-slot
/// register file mostly is.
const POINTER_PREFIX_BLOCK: usize = 8;

/// Whether `word` is a word a copying collector must treat as a **pointer**: a boxed term or a
/// cons cell ([`beam_term::prelude::is_boxed`] / [`beam_term::prelude::is_list`]). Every other
/// primary tag — `IMMED1` only — is an immediate, which is neither relocated nor a reachability
/// edge.
fn is_pointer_bearing(word: Eterm) -> bool {
    beam_term::prelude::is_boxed(word) || beam_term::prelude::is_list(word)
}

/// One past the last **pointer-bearing** word of `words` — the bound the full-root doors publish
/// and classify a register file at.
///
/// A word is pointer-bearing when it is boxed or a list ([`is_pointer_bearing`]); `NIL` and every
/// other immediate is neither, so an all-immediate file (the common case: a 1024-slot FR file a
/// process never wrote) answers `0`.
///
/// The sweep is backwards and block-wise: while the bitwise AND of a [`POINTER_PREFIX_BLOCK`]-word
/// block's words still carries the `IMMED1` primary tag in its low two bits, *every* word of the
/// block carries that tag — a bit is set in the AND only if it is set in all of them — so all of
/// them are immediates, none is boxed, none is a list, and the whole block is skipped on one
/// 64-byte read. The block is taken as a `&[Eterm; POINTER_PREFIX_BLOCK]` through `TryInto`, so
/// its eight word reads carry no per-element bounds check, and the eight raw words are ANDed in a
/// short unrolled loop.
///
/// That AND test is **exactly** the pointer-bearing test: `IMMED1` is the only primary tag that is
/// neither boxed nor a list, so a tag other than `IMMED1` means some word of the block has a low
/// tag bit clear (an AND bit is clear only if it is clear in some word) and is therefore
/// pointer-bearing. The block that stops the sweep thus holds at least one pointer-bearing word,
/// and every word above it — the whole skipped tail — is an immediate, so the file's last
/// pointer-bearing word lies **inside that block**. The single fallback `rposition` is therefore
/// bounded to it: it scans the block that stopped the sweep, or — when *every* whole block was
/// skipped — the `0..[`POINTER_PREFIX_BLOCK`]`-word tail left in front of it. The returned index
/// is identical to a full per-word scan's for every input, while the words the fallback examines
/// are at most [`POINTER_PREFIX_BLOCK`] instead of the whole file, at any file length.
///
/// See the module docs' *pointer-bearing prefix* section for why bounding a register file there
/// can never lose a root: every omitted word is an immediate, which is neither relocatable nor a
/// reachability edge, and [`root_is_unscanned`] answers `false` for it.
fn pointer_prefix_len(words: &[Eterm]) -> usize {
    #[cfg(test)]
    prefix_counters::note_call();
    let mut end = words.len();
    while end >= POINTER_PREFIX_BLOCK {
        // `words[end - POINTER_PREFIX_BLOCK..end]` is always exactly one block long here, so the
        // array conversion is infallible; the `break` arm is unreachable and exists only to keep
        // the function panic-free.
        let Ok(block) =
            <&[Eterm; POINTER_PREFIX_BLOCK]>::try_from(&words[end - POINTER_PREFIX_BLOCK..end])
        else {
            break;
        };
        // The block's eight raw words in one AND. A set bit in the result is set in every word, so
        // the `IMMED1` tag surviving means the whole block is immediate.
        let mut tags = !0usize;
        let mut offset = 0;
        while offset < POINTER_PREFIX_BLOCK {
            tags &= block[offset].raw();
            offset += 1;
        }
        if tags & beam_term::prelude::TAG_PRIMARY_MASK != beam_term::prelude::TAG_PRIMARY_IMMED1 {
            // This block stopped the sweep, so it holds the file's last pointer-bearing word.
            break;
        }
        #[cfg(test)]
        prefix_counters::note_block_skipped();
        end -= POINTER_PREFIX_BLOCK;
    }
    // `end` is the exclusive end of the only region that can still hold the answer: the block that
    // stopped the sweep (`end >= POINTER_PREFIX_BLOCK` — and that block holds a pointer-bearing
    // word, so `rposition` always finds one there and the `map_or` default is unreachable; a test
    // pins that), or the sub-block tail in front of a file whose whole blocks were all skipped
    // (`end < POINTER_PREFIX_BLOCK`).
    let start = end.saturating_sub(POINTER_PREFIX_BLOCK);
    #[cfg(test)]
    {
        if end >= POINTER_PREFIX_BLOCK {
            prefix_counters::note_block_bounded_fallback();
        } else {
            prefix_counters::note_tail_bounded_fallback(end);
        }
    }
    words[start..end]
        .iter()
        .rposition(|&word| is_pointer_bearing(word))
        .map_or(0, |index| start + index + 1)
}

/// The `(x_count, fr_count)` bound the full-root doors hand the machine's two register files over
/// at: each file's [`pointer_prefix_len`], computed once per file per door call. The Erlang stack
/// is published whole — it has no pointer-only prefix property, because every slot of a live
/// frame is a root.
fn machine_root_bounds(roots: &RootSet<'_>) -> (usize, usize) {
    (pointer_prefix_len(roots.x), pointer_prefix_len(roots.fr))
}

/// Hand the executing machine's live roots to `process` — the **publish** half of the GC
/// hand-off.
///
/// `live` is the number of leading `x_regs` slots the caller declares live;
/// `x_regs.len()` (or [`MAX_REG`], whichever is smaller) declares **every** register live,
/// which is the conservative choice when the true count is unknown: a copying collector may
/// always over-approximate its root set (it can only keep a term alive that would otherwise
/// die, never free a live one). The whole `stack` is published, because there are no frame
/// liveness maps.
///
/// Returns whether the hand-over was complete. A `false` means the process's root set could
/// not hold the machine's state intact, so the caller must **not** collect against it.
///
/// This is the single publish implementation shared by the in-slice heap-admission point
/// ([`ensure_heap`]) and the runner's **park** arm ([`crate::runner`]), which publishes a
/// suspended process's machine so that a delivery-driven collection can relocate the parked
/// heap against the machine's real roots.
///
/// # Errors
///
/// None: an impossible `set_live` is reported as `false` (the caller's "do not collect").
pub(crate) fn publish_roots(
    process: &mut Process,
    live: usize,
    x_regs: &[Eterm],
    stack: &[Eterm],
) -> bool {
    let live = live.min(x_regs.len()).min(MAX_REG);
    let regs = process.registers_mut();
    match regs.x_mut().set_live(live) {
        Ok(()) => {
            regs.x_mut().live_slice_mut()[..live].copy_from_slice(&x_regs[..live]);
            let process_stack = regs.stack_mut();
            process_stack.clear();
            for word in stack.iter() {
                process_stack.push(*word);
            }
            process.registers().stack().len() == stack.len()
        }
        Err(_) => false,
    }
}

/// Write a collected process's (possibly relocated) registers back into the machine's slices
/// — the **scatter** half of the GC hand-off.
///
/// `live` must be the same count [`publish_roots`] was called with. The collector preserves
/// the slice lengths and the word order, so a length mismatch means the machine's slices can
/// no longer be trusted: the function reports `false` and the caller must stop collecting over
/// this process rather than write a mis-sized block back.
pub(crate) fn scatter_roots(
    process: &Process,
    live: usize,
    x_regs: &mut [Eterm],
    stack: &mut [Eterm],
) -> bool {
    let regs = process.registers();
    let relocated_x = regs.x().live_slice();
    let relocated_stack = regs.stack().as_slice();
    if relocated_x.len() != live || relocated_stack.len() != stack.len() {
        return false;
    }
    x_regs[..live].copy_from_slice(relocated_x);
    stack.copy_from_slice(relocated_stack);
    true
}

/// Hand the executing machine's **whole** root set to `process` — the full-root twin of
/// [`publish_roots`].
///
/// Where the narrow [`publish_roots`] takes an *x-register count* and ignores the FR file,
/// this one takes the engine's [`RootSet`] and publishes **every** slot of the value it is handed:
///
/// * all of `roots.x` (the [`RootSet::live`] hint is deliberately **ignored** — a copying
///   collector may always over-approximate its root set, so treating every register as live
///   can only keep a term alive that would otherwise die, never free a live one, and the
///   engine has no way to prove a register dead);
/// * then every word of `roots.stack`; and
/// * then the first `fr_count` words of `roots.fr` — the machine's live float-register prefix.
///
/// *Which* value a caller hands over is the caller's bound, not this function's business: the
/// in-slice doors hand a `RootSet` already narrowed to each file's **pointer-bearing prefix**
/// ([`machine_root_bounds`]), while the park arm hands the machine's whole `x` file with the FR
/// file bounded by its own live prefix. Either way the published words are the words that can
/// carry a heap pointer, and every caller passes the very same `fr_count` to
/// [`scatter_roots_full`].
///
/// # Why the FR file rides in the stack container
///
/// The FR file holds **`Eterm` term slots** at Milestone 1 (there are no float registers in
/// this engine), so a float register can point into the process heap and must be a root — but
/// [`beam_process::Registers`] has only the x-register file and the Erlang stack, with **no**
/// FR container. Appending the FR words *after* the stack words for the duration of one
/// hand-off is the only faithful representation available: [`Process::collect`] gathers the
/// whole stack as roots and scatters the relocated words back into exactly the slots they
/// came from, so the FR words are relocated like any other root and [`scatter_roots_full`]
/// copies them back out into `roots.fr`.
///
/// This is the publish implementation shared by the in-slice heap-admission doors
/// ([`ensure_heap_full`] and [`alloc_words_rooted`]) and the runner's **park** arm, so a parked
/// machine's FR file cannot dangle either.
///
/// # `fr_count` — how much of the FR file is published
///
/// `fr_count` is the number of **leading** FR slots appended after the stack words; it is
/// clamped to `roots.fr.len()` here — and identically in [`scatter_roots_full`], so the length
/// rule cannot disagree between the two halves of a hand-off — and `0` publishes no FR word at
/// all. The two callers pass the two ends of that bound:
///
/// * the **in-slice heap-admission** doors ([`ensure_heap_full`], [`alloc_words_rooted`]) pass
///   the FR file's [`pointer_prefix_len`] ([`machine_root_bounds`]) — one past its last
///   boxed-or-list slot, `0` for the `NIL`-only file a process that never touched a float
///   hands over. The hand-off carries the machine's whole live root set: the bound omits only
///   slots that are *immediates*, which no collection could relocate or keep alive, and every
///   **live** float register holds a boxed float term and so lies inside the bound. The SCATTER
///   half (which writes renamed words back into the machine) is given the same count, so it
///   covers exactly what the publish half put on the stack;
/// * the runner's **park** arm passes the engine's
///   [`Machine::fr_mark`](beam_engine::Machine::fr_mark) — the exact number of live leading FR
///   slots, i.e. one past the highest slot a `Machine::set_fr` has written since the machine's
///   last reset. An FR slot beyond `fr_mark` is `NIL`: publishing a whole 1024-word file for
///   the overwhelming majority of parks (a process that never touched a float) roots nothing,
///   yet costs a 1024-word copy on every park and a 1024-word relocation on every collection
///   that follows it.
///
/// The two halves of the FR length rule live here and in [`scatter_roots_full`], so a
/// park/resume pair must pass the **same** `fr_count`.
///
/// # Errors
///
/// None: an impossible `set_live` is reported as `false` (the caller's "do not collect").
pub(crate) fn publish_roots_full(
    process: &mut Process,
    roots: &RootSet<'_>,
    fr_count: usize,
) -> bool {
    // Clamped identically to `scatter_roots_full`'s, so the two halves of one hand-off size
    // the FR region the same way and the length check below can never spuriously mismatch.
    let fr_count = fr_count.min(roots.fr.len());
    let live = roots.x.len().min(MAX_REG);
    let regs = process.registers_mut();
    match regs.x_mut().set_live(live) {
        Ok(()) => {
            regs.x_mut().live_slice_mut()[..live].copy_from_slice(&roots.x[..live]);
            let process_stack = regs.stack_mut();
            process_stack.clear();
            for word in roots.stack.iter() {
                process_stack.push(*word);
            }
            for word in roots.fr[..fr_count].iter() {
                process_stack.push(*word);
            }
            process.registers().stack().len() == roots.stack.len() + fr_count
        }
        Err(_) => false,
    }
}

/// Write a collected process's (possibly relocated) roots back into the machine's slices —
/// the **scatter** half of the full-root hand-off.
///
/// The exact inverse of [`publish_roots_full`]: it requires the relocated x-register prefix
/// to be as long as [`publish_roots_full`] published and the relocated stack to hold exactly
/// the stack words **plus** the FR words `fr_count` says were appended, then splits that
/// region back into `roots.stack` and `roots.fr` in the order they were appended.
///
/// `fr_count` must be the **same** value the matching [`publish_roots_full`] was called with
/// (both clamp it to `roots.fr.len()` identically); a mismatch makes the length check below
/// fail (a `false`, which halts the process's collection). The collector preserves the slice
/// lengths and the word order, so a length mismatch means the machine's slices can no longer
/// be trusted: the function reports `false` and the caller must stop collecting over this
/// process rather than write a mis-sized block back.
///
/// With `fr_count == 0` — or a `[..fr_count]` window of `NIL` slots — the FR region carries no
/// live root, so the words beyond `fr_count` are left **untouched** (the machine's FR file
/// stays exactly as it was: `NIL`).
pub(crate) fn scatter_roots_full(
    process: &Process,
    roots: &mut RootSet<'_>,
    fr_count: usize,
) -> bool {
    // Clamped identically to `publish_roots_full`'s, so one hand-off's two halves can never
    // disagree on the published length.
    let fr_count = fr_count.min(roots.fr.len());
    let x_count = roots.x.len().min(MAX_REG);
    let regs = process.registers();
    let relocated_x = regs.x().live_slice();
    let relocated_stack = regs.stack().as_slice();
    if relocated_x.len() != x_count || relocated_stack.len() != roots.stack.len() + fr_count {
        return false;
    }
    roots.x[..x_count].copy_from_slice(relocated_x);
    let stack_words = roots.stack.len();
    roots.stack.copy_from_slice(&relocated_stack[..stack_words]);
    roots.fr[..fr_count].copy_from_slice(&relocated_stack[stack_words..]);
    true
}

/// The action the heap-admission point takes once it is committed to collecting: the
/// collector's on-demand **growth** seam ([`Process::ensure_capacity`]) whenever the
/// process's own heap-size policy can actually reach `heap_need` — a plain
/// [`Process::collect`] otherwise.
///
/// The difference is that a collection sizes the nursery from what it left alive in it, not
/// from what the caller is about to allocate, so a request larger than the nursery could
/// never be met by collecting alone: the allocation would fall back to the pointer-stable
/// arena ([`alloc_words`]) on every round. Growing the now-empty nursery to cover the
/// request is what keeps such a workload on the process's own — reclaimable — heap.
///
/// `Process::ensure_capacity` fails **fast** — before it collects — when
/// `HeapPolicy::next_words(need, 0) < need`, i.e. when a bounded policy (`+hmax N`) cannot
/// reach the request at all. That case keeps the pre-growth behaviour exactly: the
/// collection still runs, and the allocation falls back to the arena — never a fabricated
/// collection, never a new failure mode.
///
/// Both heap-admission doors ([`ensure_heap`] and [`ensure_heap_full`]) take this action, so
/// the narrow and the full-root door can never drift apart on it.
///
/// # Errors
///
/// The collection's own error. The caller halts collection for that process
/// ([`Runtime::halt_process_gc`](crate::runtime::gc)) and reports success, leaving the
/// allocation to fall back to the arena exactly as it did before.
fn collect_or_grow(process: &mut Process, heap_need: u32) -> Result<Collection> {
    let need = heap_need as usize;
    let growable = process.collector().policy().next_words(need, 0) >= need;
    if growable {
        process.ensure_capacity(need)
    } else {
        process.collect()
    }
}

/// The engine's heap-admission point for a scheduled slice: collect the running process's
/// own heap when it cannot satisfy `heap_need` more words — **growing** it when the
/// request is larger than the nursery — when a `garbage_collect` BIF asked for one, or when
/// the nursery carries a growth's **collection debt** (a retired-space chain only a collection
/// can evacuate, [`Process::collection_debt`]).
///
/// `armed` is the slice latch (see the module docs). When it is not set, when the running
/// process is not collectable, or when the call site's module is not known to be float-free,
/// this is a no-op — exactly the pre-collector runtime.
///
/// # Why the caller-module guard is load-bearing *here*
///
/// This door is handed only the x-register prefix and the stack, so it can never see the
/// machine's FR file and a collection here **would** dangle a live float register. The
/// [`module_may_be_collected`] test on the call site's module is therefore the only thing that
/// keeps this door sound for float code — and it is this door's **alone**: the full doors
/// ([`ensure_heap_full`], [`alloc_words_rooted`]) publish each register file's pointer-bearing
/// prefix, the FR file's included ([`publish_roots_full`]), and apply no float guard at all (see
/// the module docs' float-guard section). The engine reaches only the full door, so this narrow
/// form is not on the engine's path today.
///
/// # Growth
///
/// The collection this runs is the collector's on-demand growth seam
/// ([`Process::ensure_capacity`]) whenever the process's own heap-size policy can actually
/// reach `heap_need` — a plain [`Process::collect`] otherwise. The difference is that a
/// collection sizes the nursery from what it left alive in it, not from what the caller is
/// about to allocate, so a request larger than the nursery could never be met by collecting
/// alone: the allocation would fall back to the pointer-stable arena ([`alloc_words`]) on
/// every round. Growing the now-empty nursery to cover the request is what keeps such a
/// workload on the process's own — reclaimable — heap.
///
/// A policy that genuinely cannot reach `heap_need` (`+hmax N` below it) is the second
/// branch: `Process::ensure_capacity` refuses such a request **before** collecting, so this
/// keeps today's behaviour exactly — the same collection runs, and the allocation falls back
/// to the arena. Never a fabricated collection, never a new failure mode.
///
/// # Forced collections
///
/// `armed` is `true` for the slice's **own** machine and nothing else, so this is one of the
/// two points that hold the root set a *requested* collection needs
/// ([`Runtime::request_gc`](crate::runtime::Runtime)): a pending request is consumed here and
/// the collection runs **whatever the heap's occupancy** — the "the nursery is full" test is
/// what a request overrides, not the safety valves. A request the valves refuse (a pinned or
/// halted pid, the global valve), or one whose call site is not float-free, is dropped
/// without a collection: the BIF already answered the oracle's `true`, and the honest
/// outcome is no collection at all, never a fabricated one.
///
/// # Errors
///
/// This deliberately has **no** failure mode that could reach the program: a collection is
/// an optimization of an unbounded arena and must never turn a working program into a
/// failing one. A failed (or mis-sized) collection — or a refused grow — halts collection
/// for that process ([`Runtime::halt_process_gc`](crate::runtime::gc)) and reports success,
/// leaving the allocation to fall back to the arena exactly as it did before.
pub(crate) fn ensure_heap(
    ctx: &mut RuntimeContext<'_>,
    heap_need: u32,
    _live: u32,
    x_regs: &mut [Eterm],
    stack: &mut [Eterm],
    armed: bool,
) -> Result<()> {
    if !armed {
        return Ok(());
    }
    let pid = ctx.mailbox_pid;
    // A `garbage_collect` request (`erlang:garbage_collect/0,1`,
    // `erts_internal:garbage_collect/1`) forces the collection. It is **peeked** here and
    // consumed only once this point is committed to collecting, so a request refused by the
    // float guard below stays armed for the runner's park arm — where the machine's
    // float-register file is *provably* dead and the collection is therefore legitimate
    // (`Runtime::serve_gc_request`).
    let forced = ctx.runtime.gc_request_pending(pid);
    if !ctx.runtime.gc_process_enabled(pid) {
        ctx.runtime.take_gc_request(pid);
        return Ok(());
    }
    // The float guard — **this door's alone** (see the module docs' float-guard section): it
    // receives only an x prefix and the stack, so it cannot see the machine's FR file and a
    // collection it drove could dangle a live float register. Only a call site whose module is
    // known to be float-free may therefore be collected over. An unknown call site (a fun
    // entry, the `-eval` evaluator) is skipped, never guessed at.
    let Some(name) = ctx.current_module.clone() else {
        return Ok(());
    };
    if !module_may_be_collected(ctx.runtime, &name) {
        return Ok(());
    }

    // The two disjoint fields: the process table (the caller's, for a slice) and the
    // runtime. Destructuring the context is what lets both be borrowed at once.
    let RuntimeContext {
        runtime,
        mailbox_table,
        ..
    } = ctx;
    let table: &mut ProcessTable = match mailbox_table {
        Some(table) => table,
        // Not a scheduled slice: the runtime's own process has no movable heap.
        None => return Ok(()),
    };
    let Some(process) = table.get_mut(pid) else {
        return Ok(());
    };
    // The **occupancy** test, plus the collector's **collection debt** (see
    // [`ensure_heap_full`]): a nursery chain a no-collect growth retained forces the collection
    // even when the current space still has room.
    let debt = process.collection_debt();
    let occupancy = process.should_collect(heap_need as usize);
    if !forced && !occupancy && !debt {
        return Ok(());
    }
    #[cfg(test)]
    valve_counters::note_reached(
        valve_counters::Door::EnsureHeap,
        occupancy,
        debt,
        forced,
        x_regs.len() + stack.len(),
    );
    // The **unscanned-root valve** (see [`root_is_unscanned`]): a live root in a region the
    // collector does not scan would be left pointing at young words this move relocates, so the
    // move is **declined** — the heap keeps its terms, the unscanned root stays valid and the
    // program stays correct. The test runs *after* the occupancy short-circuit (an ordinary
    // admission point that is not about to collect pays nothing) and *before* the request is
    // consumed, so a forced collection it refuses stays armed for the runner's park arm.
    //
    // `descend` is the pid's **foreign-reachability gate**
    // ([`Runtime::foreign_reachable`](crate::runtime)): the per-word root phase always runs, so
    // a root that is *itself* a foreign container is still declined here without any descent,
    // and the descent — the `O(live objects)` half — is only paid for a process that has ever
    // held one (see the valve's module docs). A **hit** opens the gate permanently: a foreign
    // container really did reach a live root, which is the event the gate records.
    let descend = runtime.foreign_reachable(pid);
    let declined = any_root_is_unscanned(
        runtime.walk_scratch(),
        process,
        x_regs.iter().chain(stack.iter()),
        descend,
    );
    #[cfg(test)]
    valve_counters::note_valve(valve_counters::Door::EnsureHeap, descend, declined);
    #[cfg(test)]
    if declined {
        valve_counters::note_decline(
            valve_counters::Door::EnsureHeap,
            valve_counters::classify_decline(
                runtime.walk_scratch(),
                process,
                x_regs.iter().chain(stack.iter()),
            ),
        );
        // This door has no FR slice of its own; the empty slice simply never matches.
        valve_counters::note_first_decline(
            valve_counters::Door::EnsureHeap,
            x_regs,
            stack,
            &[],
            (_live as usize).min(x_regs.len()),
            process,
        );
    }
    if declined {
        runtime.note_foreign_reachable(pid);
        return Ok(());
    }
    // Committed: the request is answered exactly once, by this collection.
    if forced {
        runtime.take_gc_request(pid);
    }

    // Transfer the machine's roots into the process root set. `Process::collect` gathers
    // exactly these containers (registers, then the mailbox, dictionary, saved arguments and
    // pending signals), so publishing them here is what makes the machine's live terms survive
    // the move; the relocated words are copied back below.
    //
    // The `live` argument is deliberately **ignored**: this door receives only the x registers
    // and the stack, so it cannot see the FR file, and the engine's live count is only a hint.
    // Treating the *whole* x file as live is the conservative direction a copying collector
    // always allows (it can only keep a term alive, never free a live one), and it is what
    // keeps a register the engine has declared dead but still reads from going stale.
    let live = x_regs.len().min(MAX_REG);
    if !publish_roots(process, live, x_regs, stack) {
        // The root set could not be handed over intact: keep this process's heap put rather
        // than collect against a partial root set.
        runtime.halt_process_gc(pid);
        return Ok(());
    }

    // The action: a plain collection, or — when the process's own heap-size policy can
    // actually reach `heap_need` — the collector's on-demand **growth** seam. See
    // [`collect_or_grow`] for why the growth is what keeps a request larger than the nursery
    // on the process's own reclaimable heap, and for the bounded-policy fallback.
    let action = collect_or_grow(process, heap_need);
    match action {
        Ok(collection) => {
            runtime.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
            runtime.note_gc_reclaimed(collection.stats.words_reclaimed);
            #[cfg(test)]
            valve_counters::note_collected(valve_counters::Door::EnsureHeap);
        }
        Err(_) => {
            runtime.halt_process_gc(pid);
            return Ok(());
        }
    }

    // Copy the relocated roots back into the machine's slices, in place: the collector
    // preserves the slice lengths and the word order, so every `Machine::frames` `y_base`
    // index and every frame slot stays valid.
    if !scatter_roots(process, live, x_regs, stack) {
        runtime.halt_process_gc(pid);
        return Ok(());
    }
    Ok(())
}

/// The engine's heap-admission point for a scheduled slice, taking the machine's **whole**
/// live root set ([`RootSet`]: the x-register file, the whole Erlang stack **and the FR
/// file**, each register file bounded to its pointer-bearing prefix — see below).
///
/// This is the full-root twin of [`ensure_heap`], reached by
/// `Machine::ensure_heap`/`test_heap`/`allocate_heap` through
/// [`ExecutionContext::ensure_heap_full`](beam_engine::ExecutionContext::ensure_heap_full).
/// The gate order is the narrow door's — the latch, the forced-collection request, the
/// collectability test, the "this door has a caller module" test, the table/process lookup, the
/// occupancy-or-debt test and the "committed" request consume — and only the root hand-off
/// differs: it publishes and scatters `roots` — the whole stack plus each register file's
/// [`pointer_prefix_len`] ([`machine_root_bounds`]) — through
/// [`publish_roots_full`]/[`scatter_roots_full`] instead of a `live`-bounded x prefix.
/// It applies **no caller-module float guard**: because the FR file's pointer-bearing prefix is
/// published (and a live float register is a boxed float term, hence inside it), this door is
/// **FR-safe for any module** — a live float register is relocated like any other root — so a
/// float-using call site collects here exactly like a float-free one (see the module docs'
/// float-guard section).
///
/// The occupancy test is OR-ed with the collector's **collection debt**
/// ([`Process::collection_debt`]): a nursery chain a no-collect [`Process::grow_nursery`]
/// growth retained is treated like a full nursery, because only a collection can evacuate it.
///
/// # Growth
///
/// This door takes the **same action** as the narrow one ([`collect_or_grow`]): the
/// collector's on-demand growth seam whenever the process's own heap-size policy can reach
/// `heap_need`, a plain collection otherwise. A request larger than the nursery therefore
/// grows the (now-empty) nursery to fit it instead of leaving every round to the
/// pointer-stable arena, and a bounded policy (`+hmax N`) that cannot reach the request
/// keeps the pre-growth behaviour — the collection still runs and the allocation falls back
/// to the arena. See [`collect_or_grow`] for the full rationale.
///
/// # Why the whole root set, not the engine's `live` hint
///
/// The narrow door receives only `x_regs`/`stack` and a `live` count; this one receives the
/// FR file too, and ignores the hint (see [`publish_roots_full`]). Over-approximating a
/// copying collector's root set is always safe, and the engine's `live` count is a hint the
/// collector must not trust: an x slot the engine has declared dead can still be read later
/// in the same run, so leaving it un-relocated makes the heap move out from under it. Bounding
/// the file to its **pointer-bearing prefix** is not the same as trusting `live`: a
/// dead-but-still-read slot holding a term is *boxed*, so it stays inside the bound, and only
/// a slot that is an immediate is omitted — a word no later read can turn into a heap pointer
/// (the engine writes a *fresh* pointer into a slot, and the next admission point re-derives
/// the bound from the machine's then-current registers).
///
/// # Errors
///
/// As [`ensure_heap`]: no failure mode reaches the program. A failed (or mis-sized)
/// collection halts collection for that process
/// ([`Runtime::halt_process_gc`](crate::runtime::gc)) and reports success.
pub(crate) fn ensure_heap_full(
    ctx: &mut RuntimeContext<'_>,
    heap_need: u32,
    roots: RootSet<'_>,
    armed: bool,
) -> Result<()> {
    if !armed {
        return Ok(());
    }
    let pid = ctx.mailbox_pid;
    let forced = ctx.runtime.gc_request_pending(pid);
    if !ctx.runtime.gc_process_enabled(pid) {
        ctx.runtime.take_gc_request(pid);
        return Ok(());
    }
    // The door needs a caller module to name at all — a fun entry, or the `-eval` evaluator,
    // has none — and is otherwise left alone. This is **not** a float guard: this door publishes
    // each register file's pointer-bearing prefix ([`publish_roots_full`]), the FR file's
    // included, so a live float register — which holds a boxed float term, hence lies inside the
    // prefix — is relocated like any other root and a float-using call site collects exactly like
    // a float-free one (see the module docs' float-guard section).
    if ctx.current_module.is_none() {
        return Ok(());
    }
    let RuntimeContext {
        runtime,
        mailbox_table,
        ..
    } = ctx;
    let table: &mut ProcessTable = match mailbox_table {
        Some(table) => table,
        None => return Ok(()),
    };
    let Some(process) = table.get_mut(pid) else {
        return Ok(());
    };
    // The **occupancy** test, plus the collector's **collection debt**. The debt term is what a
    // no-collect [`Process::grow_nursery`] growth leaves behind: it retires the young space it
    // replaces instead of freeing it, so the nursery becomes a chain holding everything the
    // earlier spaces ever allocated until a real collection evacuates it. A retained chain
    // therefore forces the collection even when the *current* space still has room — without
    // the term, a growth-shaped workload keeps its whole allocation history resident.
    //
    // The two terms stay disjoint: [`Process::should_collect`] is the occupancy test alone
    // (pinned as such), and the debt is the growth seam's own signal.
    let debt = process.collection_debt();
    let occupancy = process.should_collect(heap_need as usize);
    if !forced && !occupancy && !debt {
        return Ok(());
    }
    // The **pointer-bearing prefix** of each register file, computed **once** (see the module
    // docs' *pointer-bearing prefix* section): one past the last boxed-or-list word of the x file
    // and of the FR file, or `0` for a file of immediates. Both the words this call classifies and
    // the words it hands over are bounded by these two counts, so the valve can never see a word
    // the hand-off does not carry, and vice versa. The stack is always whole.
    let (x_count, fr_count) = machine_root_bounds(&roots);
    #[cfg(test)]
    valve_counters::note_reached(
        valve_counters::Door::EnsureHeapFull,
        occupancy,
        debt,
        forced,
        roots.x.len() + roots.stack.len() + roots.fr.len(),
    );
    #[cfg(test)]
    valve_counters::note_root_words(
        valve_counters::Door::EnsureHeapFull,
        roots.x.len() + roots.stack.len() + roots.fr.len(),
        x_count + roots.stack.len() + fr_count,
    );
    // The **unscanned-root valve** — the full-root twin of the one in [`ensure_heap`]: a root in
    // a region the collector does not scan declines the move (see [`root_is_unscanned`]), so the
    // heap keeps its terms and the unscanned root stays valid. Tested after the occupancy
    // short-circuit and before the request is consumed, so a refused forced collection stays
    // armed for the runner's park arm. The descent is gated on `pid`'s foreign-reachability bit
    // and a hit opens it permanently, exactly as in [`ensure_heap`].
    //
    // The iterator is the bounded one, which is the *same* decision the full file would give:
    // [`root_is_unscanned`] answers `false` for every word past the last pointer (they are
    // immediates), so the bound can only skip work, never change the verdict.
    let descend = runtime.foreign_reachable(pid);
    let machine_roots = || {
        roots.x[..x_count]
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr[..fr_count].iter())
    };
    let verdict = valve_verdict(runtime.walk_scratch(), process, machine_roots(), descend);
    let declined = verdict.declined;
    #[cfg(test)]
    valve_counters::note_valve(valve_counters::Door::EnsureHeapFull, descend, declined);
    #[cfg(test)]
    if declined {
        valve_counters::note_decline(
            valve_counters::Door::EnsureHeapFull,
            valve_counters::classify_decline(runtime.walk_scratch(), process, machine_roots()),
        );
        let live_len = roots.live_len();
        valve_counters::note_first_decline(
            valve_counters::Door::EnsureHeapFull,
            roots.x,
            roots.stack,
            roots.fr,
            live_len,
            process,
        );
    }
    if declined {
        runtime.note_foreign_reachable(pid);
        return Ok(());
    }
    // The **latch's closing half** (see [`root_is_unscanned`] and
    // [`Runtime::close_foreign_reachable`]): this door handed the valve the machine's whole root
    // set, and the descent it ran reached no container from a region the collector does not scan
    // — so the process's graph holds none, and the descent can be skipped again until an event
    // puts one back. No other door may do this: [`ensure_heap`] cannot see the FR file, and the
    // park arm and the signal gate run for a process whose roots are not published.
    if verdict.foreign_free {
        runtime.close_foreign_reachable(pid);
    }
    if forced {
        runtime.take_gc_request(pid);
    }

    // Hand the machine's root set over at exactly those bounds: this door hands it over while the
    // machine is running, so it cannot bound the live prefix — but it need not, because every
    // omitted word is an immediate, which no collection relocates and no reachability test reads.
    // The narrowed `RootSet` is what makes the two halves (publish and scatter) use **one** length
    // rule: each derives its counts from the slice lengths of the value it is handed, and both are
    // handed the same narrowed value (see [`publish_roots_full`]).
    let RootSet { x, stack, fr, live } = roots;
    let mut roots = RootSet::new(&mut x[..x_count], stack, &mut fr[..fr_count], live);
    if !publish_roots_full(process, &roots, fr_count) {
        runtime.halt_process_gc(pid);
        return Ok(());
    }

    // The action is the same as the narrow door's: collect, or grow the nursery to fit a
    // request the post-collection nursery cannot hold. See [`collect_or_grow`].
    let action = collect_or_grow(process, heap_need);
    match action {
        Ok(collection) => {
            runtime.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
            runtime.note_gc_reclaimed(collection.stats.words_reclaimed);
            #[cfg(test)]
            valve_counters::note_collected(valve_counters::Door::EnsureHeapFull);
        }
        Err(_) => {
            runtime.halt_process_gc(pid);
            return Ok(());
        }
    }

    if !scatter_roots_full(process, &mut roots, fr_count) {
        runtime.halt_process_gc(pid);
        return Ok(());
    }
    Ok(())
}

/// Heap words still available to the running code.
///
/// In a scheduled slice with collection enabled this is the **real** figure — the running
/// process's young free words — because that is the heap the code allocates from. Every
/// other context reports the runtime's arena, which is unbounded and never reclaimed.
pub(crate) fn heap_available(ctx: &RuntimeContext<'_>) -> usize {
    if ctx.mailbox_table.is_none() {
        return ctx.runtime.arena_available();
    }
    let pid = ctx.mailbox_pid;
    if !ctx.runtime.gc_process_enabled(pid) {
        return ctx.runtime.arena_available();
    }
    match ctx.process_table().get(pid) {
        Some(process) => process.heap().available(),
        None => ctx.runtime.arena_available(),
    }
}

/// Allocates `n` raw heap words for the executing code.
///
/// In a scheduled slice with collection enabled the block comes from the running process's
/// young heap, so it is reclaimed by the collector once it is unreachable and the peak memory
/// tracks the live set.
///
/// Everything **else** — an `-eval`/single-pass context (no process table), a pinned or halted
/// process, a process the table no longer holds, and a request the young heap cannot meet even
/// after a growth — keeps the pointer-stable arena, byte for byte as before this seam existed.
/// A fallback is never an error: the arena cannot fail where the young heap could.
///
/// # Grow-and-retry before the arena
///
/// This seam is the **unrooted** allocation path (`beam_engine::terms::sink::ContextSink`,
/// metadata-only builds): it is handed **no** root set, so it may neither collect nor hand a
/// collection roots. A young-heap refusal that is a **capacity** failure is therefore met by
/// [`Process::grow_nursery`] — the **no-relocation** growth seam, which retires the space it
/// replaces without moving a single live term, so it is safe against a root set the caller
/// cannot publish — and the bump is then retried. Only when the growth itself fails (the
/// policy's ceiling cannot reach the request) or the retried bump still does not fit does the
/// block come from the pointer-stable arena, byte for byte as before this seam existed. A
/// refusal that is **not** a capacity failure (an impossible request past
/// [`MAX_ARITYVAL`](beam_term::MAX_ARITYVAL)) goes straight to the arena: growth cannot fix it.
///
/// Because growth retains the space it replaces, a root can point into a *retired* nursery
/// space; the admission-point valve ([`root_is_unscanned`]) accepts a pointer into **any**
/// nursery space for exactly that reason, so such a root never wrongly declines a collection.
///
/// # Errors
///
/// Propagates the arena's allocation failure only.
pub(crate) fn alloc_words(ctx: &mut RuntimeContext<'_>, n: usize) -> Result<*mut Eterm> {
    let RuntimeContext {
        runtime,
        mailbox_table,
        mailbox_pid,
        ..
    } = ctx;
    if let Some(table) = mailbox_table {
        if runtime.gc_process_enabled(*mailbox_pid) {
            if let Some(process) = table.get_mut(*mailbox_pid) {
                // The first attempt: the pool's placeholder fill is scoped so its `&mut runtime`
                // borrow has ended before any later `runtime` call.
                let first = {
                    let fill = runtime.gc_scratch(n.saturating_sub(1));
                    bump(process.heap_mut(), n, fill)
                };
                match first {
                    Ok(block) => return Ok(block),
                    // A capacity refusal: grow the nursery (no relocation, no collection) and
                    // retry the bump. Growth that cannot reach the request, or a retry that
                    // still does not fit, falls through to the arena.
                    Err(BeamError::Alloc(_)) => {
                        if process.grow_nursery(n).is_ok() {
                            let fill = runtime.gc_scratch(n.saturating_sub(1));
                            if let Ok(block) = bump(process.heap_mut(), n, fill) {
                                return Ok(block);
                            }
                        }
                    }
                    // Any other refusal (an impossible request) cannot be fixed by growth.
                    Err(_) => {}
                }
            }
            #[cfg(test)]
            valve_counters::note_fallback_at(valve_counters::FallbackSite::UnrootedSeam);
            runtime.note_gc_fallback(*mailbox_pid);
        }
    }
    runtime.alloc_words(n)
}

/// Allocates `n` raw heap words with the machine's **whole live root set** handed in, so a
/// context whose heap must be collected can collect as part of the allocation.
///
/// This is the collecting twin of [`alloc_words`], reached by
/// [`Machine::alloc_rooted`](beam_engine::Machine::alloc_rooted) (and therefore by the
/// engine's `terms::reserve`/`reserve_slack`) through
/// [`ExecutionContext::alloc_words_rooted`](beam_engine::ExecutionContext::alloc_words_rooted).
///
/// * **The latch is not set** (every nested run — a cross-module callee's second machine, a
///   fun the engine applied, the `-eval` evaluator): behave *exactly* like [`alloc_words`] —
///   serve the young heap or fall back to the arena, and never collect. The machine that owns
///   a nested run's roots is not the one this context was armed for, so collecting there would
///   relocate a heap against the wrong root set.
/// * **The latch is set** (the slice's own machine): serve the young heap exactly as
///   [`alloc_words`] does, retrying a **capacity** refusal through the no-relocation
///   [grow-and-retry](#grow-and-retry-before-the-valve) seam, and run one collection over the
///   handed-in roots and retry the bump when that bump **fails** — or immediately, when a
///   **forced** collection has been requested (below). A collection or scatter failure halts
///   collection for that process
///   ([`Runtime::halt_process_gc`](crate::runtime::gc)) and falls back to the arena. This door
///   applies **no** caller-module float guard — it hands the machine's **whole live root set**
///   over, each register file bounded only to its pointer-bearing prefix
///   ([`machine_root_bounds`], which always contains every live float register, since a live
///   float register is a boxed float term), so a live float register is relocated, not corrupted
///   (see the module docs' float-guard section) — and it requires only that the run has a caller
///   module to name at all, exactly as in [`ensure_heap_full`].
///
/// # Grow-and-retry before the valve
///
/// A **capacity** refusal of the young-heap bump (or a request the policy's ceiling cannot
/// reach) is met by [`Process::grow_nursery`] and a retried bump, exactly as in
/// [`alloc_words`] — a non-forced attempt only. The growth is the **no-relocation** seam: it
/// retires the space it replaces without moving a live term, so it is legal for a
/// relocation-exempt process and cannot make a later collection less conservative. It leaves
/// [`Process::collection_debt`] behind, which the next admission point's occupancy test
/// ([`ensure_heap_full`]) turns into a real collection.
///
/// Serving the block from the process's own — reclaimable — heap is what keeps an arena term
/// out of the machine's registers: the pointer-stable arena is a region the collector does
/// not scan, so a block handed out there and written into an x register is a root the
/// unscanned-root valve ([`root_is_unscanned`]) must decline every later collection over.
/// Growth that cannot reach the request, or a retry that still does not fit, leaves the
/// existing valve + arena path byte for byte as it is.
///
/// # Forced collections
///
/// A forced request ([`Runtime::request_gc`](crate::runtime::Runtime),
/// `erlang:garbage_collect/0,1` and `erts_internal:garbage_collect/1`) is honoured here
/// **before** the young-heap attempt, so this door's gate order matches
/// [`ensure_heap_full`]'s: latch → forced request → valves → caller-module test →
/// commit-once → collect. `ensure_heap_full` needs the peek because a forced request must
/// bypass its *occupancy* test; this door needs it because the bump **is** its test — a
/// request armed before an allocation the young heap can serve would otherwise be carried,
/// unserved, to the pass boundary and pruned. When the request is armed the bump is
/// bypassed, the whole root set is published, one collection runs and the request is
/// consumed exactly once; every refusal (the valves, the missing caller module, the
/// unscanned-root valve) leaves the request armed, so the runner's park arm
/// ([`Runtime::serve_gc_request`](crate::runtime::gc)) may still serve it.
///
/// # Errors
///
/// Propagates the arena's allocation failure only — never a collection failure.
pub(crate) fn alloc_words_rooted(
    ctx: &mut RuntimeContext<'_>,
    n: usize,
    roots: RootSet<'_>,
    armed: bool,
) -> Result<*mut Eterm> {
    let pid = ctx.mailbox_pid;
    // The latch is not set: every nested run — a cross-module callee's second machine, a fun
    // the engine applied, the `-eval` evaluator — keeps `alloc_words`' arena fallback, byte
    // for byte, and never collects, and it consumes **no** forced request: the machine that
    // owns a nested run's roots is not the one this context was armed for.
    if !armed {
        let RuntimeContext { runtime, .. } = ctx;
        return runtime.alloc_words(n);
    }
    // A fun entry or the `-eval` evaluator has no call site at all (`current_module`), and is
    // equally left alone: the door has no machine to name.
    let call_site_known = ctx.current_module.is_some();
    let RuntimeContext {
        runtime,
        mailbox_table,
        ..
    } = ctx;
    let Some(table) = mailbox_table else {
        return runtime.alloc_words(n);
    };
    // A **forced** collection request (see [`Runtime::request_gc`](crate::runtime::Runtime))
    // is peeked here, *before* the young-heap attempt — this door's analog of
    // [`ensure_heap_full`]'s occupancy test, which a forced request bypasses. The two
    // full-root doors therefore honour the request in the same order: latch → request →
    // valves → caller module → commit-once. The peek is gated on the call site for the same
    // reason the collection is: a run with no module to collect for must not consume a
    // request it can never serve.
    let forced = call_site_known && runtime.gc_request_pending(pid);
    if !runtime.gc_process_enabled(pid) {
        // The valves refuse the pid outright (a post-slice float pin, a halt from an earlier
        // failure, or the runtime-wide escape-store valve): the request can never be served,
        // so it is dropped rather than left armed for a valve that may never open again —
        // exactly as the two `ensure_heap` doors drop it — and the block comes from the
        // pointer-stable arena, byte for byte as before.
        runtime.take_gc_request(pid);
        return runtime.alloc_words(n);
    }
    // The same attempt `alloc_words` makes: the running process's own young heap. A forced
    // request **bypasses** it: the block is the collection's to serve, and a bump taken first
    // would end this call with the request still armed — the exact way a
    // `erlang:garbage_collect()` from a process whose only allocation goes through this seam
    // used to be carried, silently, to the pass boundary.
    if !forced {
        if let Some(process) = table.get_mut(pid) {
            // The first attempt, scoped so the placeholder fill's `&mut runtime` borrow has
            // ended before the growth call below makes its own.
            let first = {
                let fill = runtime.gc_scratch(n.saturating_sub(1));
                bump(process.heap_mut(), n, fill)
            };
            match first {
                Ok(block) => return Ok(block),
                // A **capacity** refusal: grow the nursery and retry the bump, exactly as
                // [`alloc_words`] does. [`Process::grow_nursery`] is the **no-relocation**
                // growth seam — it retires the space it replaces without moving a live term,
                // so it is legal for a relocation-exempt process and cannot make a later
                // collection less conservative — and it leaves
                // [`Process::collection_debt`], which the next admission point's occupancy
                // test turns into a real collection. Growth that cannot reach the request
                // (the policy's ceiling, `+hmax N`), or a retry that still does not fit,
                // falls through to the valve + arena path byte for byte as before.
                Err(BeamError::Alloc(_)) => {
                    if process.grow_nursery(n).is_ok() {
                        let fill = runtime.gc_scratch(n.saturating_sub(1));
                        if let Ok(block) = bump(process.heap_mut(), n, fill) {
                            return Ok(block);
                        }
                    }
                }
                // Any other refusal (an impossible request past `MAX_ARITYVAL`) cannot be
                // fixed by growth.
                Err(_) => {}
            }
        }
    }
    // The young heap refused (or a forced request bypassed it). Only the slice's own machine
    // may collect over the roots it handed in; every other context keeps `alloc_words`' arena
    // fallback, byte for byte.
    if !call_site_known {
        #[cfg(test)]
        valve_counters::note_fallback_at(valve_counters::FallbackSite::NoCallSite);
        runtime.note_gc_fallback(pid);
        return runtime.alloc_words(n);
    }
    let Some(process) = table.get_mut(pid) else {
        #[cfg(test)]
        valve_counters::note_fallback_at(valve_counters::FallbackSite::NoCallSite);
        runtime.note_gc_fallback(pid);
        return runtime.alloc_words(n);
    };
    // The unscanned-root valve (see [`root_is_unscanned`]): collecting over a root set that
    // contains a container in a region the collector does not scan would relocate the young
    // words it reaches without updating it, so this collection is declined and the block comes
    // from the pointer-stable arena — the same honest fallback every other refusal path takes.
    // The descent is gated on `pid`'s foreign-reachability bit and a hit opens it permanently,
    // exactly as in [`ensure_heap`]. A declined collection leaves a forced request **armed**,
    // exactly as at [`ensure_heap_full`], so the runner's park arm may still serve it.
    // The **pointer-bearing prefix** of each register file, computed once and used for the valve's
    // iterator *and* for both halves of the hand-off — exactly as at [`ensure_heap_full`] (see the
    // module docs' *pointer-bearing prefix* section for why the bound loses no root).
    let (x_count, fr_count) = machine_root_bounds(&roots);
    let descend = runtime.foreign_reachable(pid);
    #[cfg(test)]
    valve_counters::note_reached(
        valve_counters::Door::AllocWordsRooted,
        false,
        false,
        forced,
        roots.x.len() + roots.stack.len() + roots.fr.len(),
    );
    #[cfg(test)]
    valve_counters::note_root_words(
        valve_counters::Door::AllocWordsRooted,
        roots.x.len() + roots.stack.len() + roots.fr.len(),
        x_count + roots.stack.len() + fr_count,
    );
    let machine_roots = || {
        roots.x[..x_count]
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr[..fr_count].iter())
    };
    let verdict = valve_verdict(runtime.walk_scratch(), process, machine_roots(), descend);
    let declined = verdict.declined;
    #[cfg(test)]
    valve_counters::note_valve(valve_counters::Door::AllocWordsRooted, descend, declined);
    #[cfg(test)]
    if declined {
        valve_counters::note_decline(
            valve_counters::Door::AllocWordsRooted,
            valve_counters::classify_decline(runtime.walk_scratch(), process, machine_roots()),
        );
        let live_len = roots.live_len();
        valve_counters::note_first_decline(
            valve_counters::Door::AllocWordsRooted,
            roots.x,
            roots.stack,
            roots.fr,
            live_len,
            process,
        );
    }
    if declined {
        runtime.note_foreign_reachable(pid);
        #[cfg(test)]
        valve_counters::note_fallback_at(valve_counters::FallbackSite::ValveDeclined);
        runtime.note_gc_fallback(pid);
        return runtime.alloc_words(n);
    }
    // The latch's closing half, exactly as at [`ensure_heap_full`]: this door also handed the
    // valve the machine's whole root set, so a descent that reached no foreign container says the
    // process's graph holds none and the bit can be retired.
    if verdict.foreign_free {
        runtime.close_foreign_reachable(pid);
    }
    // From here the collection is committed to, so a pending forced request is consumed
    // **once**, at the commit point — the single `take_gc_request` [`ensure_heap_full`]
    // performs. A refusal above leaves it armed; every path below has already collected (or
    // halted), so the request is spent either way.
    if forced {
        runtime.take_gc_request(pid);
    }
    // The machine's root set is handed over at the same two bounds the valve classified: the
    // narrowed `RootSet` is what makes the publish and scatter halves share one length rule, and
    // the omitted tail is immediate-only, so nothing a collection could relocate is left behind.
    let RootSet { x, stack, fr, live } = roots;
    let mut roots = RootSet::new(&mut x[..x_count], stack, &mut fr[..fr_count], live);
    if !publish_roots_full(process, &roots, fr_count) {
        runtime.halt_process_gc(pid);
        #[cfg(test)]
        valve_counters::note_fallback_at(valve_counters::FallbackSite::Other);
        runtime.note_gc_fallback(pid);
        return runtime.alloc_words(n);
    }
    match process.collect() {
        Ok(collection) => {
            runtime.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
            runtime.note_gc_reclaimed(collection.stats.words_reclaimed);
            #[cfg(test)]
            valve_counters::note_collected(valve_counters::Door::AllocWordsRooted);
        }
        Err(_) => {
            runtime.halt_process_gc(pid);
            #[cfg(test)]
            valve_counters::note_fallback_at(valve_counters::FallbackSite::Other);
            runtime.note_gc_fallback(pid);
            return runtime.alloc_words(n);
        }
    }
    if !scatter_roots_full(process, &mut roots, fr_count) {
        runtime.halt_process_gc(pid);
        #[cfg(test)]
        valve_counters::note_fallback_at(valve_counters::FallbackSite::Other);
        runtime.note_gc_fallback(pid);
        return runtime.alloc_words(n);
    }
    // The collection emptied (or resized) the nursery: retry the bump.
    let fill = runtime.gc_scratch(n.saturating_sub(1));
    if let Ok(block) = bump(process.heap_mut(), n, fill) {
        return Ok(block);
    }
    #[cfg(test)]
    valve_counters::note_fallback_at(valve_counters::FallbackSite::PostCollection);
    runtime.note_gc_fallback(pid);
    runtime.alloc_words(n)
}

/// **Test-only** deterministic tallies for the unscanned-root valve's four doors.
///
/// The valve's per-word classification phase ([`Walk::seed`](crate::context::gc_walk) over the
/// caller's machine roots plus `Walk::seed_process_roots`) runs on **every** candidate
/// collection; the occupancy short-circuit in front of it is the only thing that keeps an
/// ordinary admission point off it (see [`ensure_heap`] and `gc_walk`'s module docs). Whether
/// that phase dominates a workload is therefore a question about two structural facts — **how
/// often a door is reached** and **how many root words each reach classifies** — never about
/// timings. These counters answer exactly those two questions, per door, so a test can pin the
/// mechanism rather than a wall clock.
///
/// They follow the crate's counter convention (`src/runner/run.rs`'s `RUN_SLICE_COUNTERS`,
/// `src/runtime/scheduler.rs`'s `PASS_SLEEPS`): a `#[cfg(test)] thread_local!` `Cell` with a
/// reset/read accessor, so the tallies exist **only in test builds** (a release binary pays
/// nothing) and a test reads its own thread's tally between a reset and the load it measures.
#[cfg(test)]
pub(crate) mod valve_counters {
    use std::cell::Cell;

    use beam_term::Eterm;
    /// The four valve doors, so a tally can name the one it came from.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Door {
        /// [`super::ensure_heap`] — the narrow admission point (no engine call site today).
        EnsureHeap,
        /// [`super::ensure_heap_full`] — the engine's `test_heap`/`allocate_heap` door.
        EnsureHeapFull,
        /// [`super::alloc_words_rooted`] — the engine's rooted term-construction seam.
        AllocWordsRooted,
        /// [`crate::runtime::gc::Runtime::serve_gc_request`] — the runner's park arm.
        ServeGcRequest,
    }

    /// One door's tallies.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct DoorCounts {
        /// Valve calls reached — the door is past its own pre-valve gates (the occupancy
        /// short-circuit for the admission doors, the young-heap bump for
        /// [`super::alloc_words_rooted`], the relocation-exempt test for the park arm).
        pub(crate) reached: usize,
        /// How many of those had `should_collect(heap_need)` true (occupancy alone).
        pub(crate) occupancy: usize,
        /// How many had the collector's **collection debt** (`Process::collection_debt`).
        pub(crate) debt: usize,
        /// How many had a forced-GC request pending.
        pub(crate) forced: usize,
        /// How many ran with `pid`'s descent latch **open**.
        pub(crate) latch_open: usize,
        /// How many the valve **declined** (its `note_foreign_reachable` arm).
        pub(crate) hits: usize,
        /// Declines attributable to a **machine root word** — an x register, a stack slot or an
        /// FR slot that is itself a container from a region the collector does not scan. This is
        /// the engine *holding* an arena/BIF-heap/fragment term, and it is the shape the arena
        /// fallback creates (the engine writes the block it was handed into a register).
        pub(crate) machine_root_hits: usize,
        /// Declines where the machine roots were clean but a **process root** (a register the
        /// process's own root set holds, a mailbox payload/from/token, a dictionary entry, a
        /// saved argument or a pending signal) is itself an unscanned container — a phase-1 hit
        /// needing no descent.
        pub(crate) process_root_hits: usize,
        /// Declines only the `O(live objects)` **descent** found — the nested shape the gate
        /// exists for.
        pub(crate) descent_hits: usize,
        /// How many ran the `O(live objects)` descent (latch open and no phase-1 hit).
        pub(crate) descents: usize,
        /// How many ran a real collection.
        pub(crate) collections: usize,
        /// The **machine root words** the door's caller *declared* at those calls — the whole
        /// x-register file, the whole Erlang stack and the whole FR file for the full-root doors
        /// (`x.len() + stack.len() + fr.len()`), the x prefix plus the stack for the narrow one.
        /// This is the size of the root set the door was handed, **not** the size of the phase-1
        /// seed loop: since the full doors bound each register file to its
        /// [`pointer_prefix_len`](super::pointer_prefix_len), the words actually classified and
        /// published are [`Self::published_words`].
        pub(crate) machine_words: usize,
        /// The same word count, repeated explicitly as the **declared** figure (see
        /// [`Self::machine_words`]): recorded together with [`Self::published_words`] by
        /// `note_root_words`, so one measurement pins both halves of the bound.
        pub(crate) declared_words: usize,
        /// The machine root words the door actually **handed over** — the pointer-bearing x
        /// prefix plus the **whole** Erlang stack plus the pointer-bearing FR prefix
        /// (`x_count + stack.len() + fr_count`). This is the exact size of phase 1's
        /// `Walk::seed` loop over the caller's own root set, and of the region
        /// [`super::publish_roots_full`] pushed onto the process stack.
        pub(crate) published_words: usize,
    }

    impl DoorCounts {
        /// All-zero tallies (the `const` form the thread-local's initialiser needs).
        const ZERO: DoorCounts = DoorCounts {
            reached: 0,
            occupancy: 0,
            debt: 0,
            forced: 0,
            latch_open: 0,
            hits: 0,
            machine_root_hits: 0,
            process_root_hits: 0,
            descent_hits: 0,
            descents: 0,
            collections: 0,
            machine_words: 0,
            declared_words: 0,
            published_words: 0,
        };
    }

    /// Which refusal sent an allocation to the pointer-stable arena.
    ///
    /// The **entry point** of the valve's self-sustaining decline circle is one of these: an arena
    /// block is a container in a region the collector does not scan, so once the engine writes one
    /// into a machine register the *next* admission point's phase 1 finds it as a root and declines
    /// — which sends the next allocation to the arena in turn. Naming the first site is what says
    /// whether the circle can be broken from the allocation seam.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum FallbackSite {
        /// The **unrooted** seam ([`super::alloc_words`]): the young heap refused and the
        /// no-relocation growth could not reach the request.
        UnrootedSeam,
        /// The rooted seam has no call site / no process table / no pid to collect for.
        NoCallSite,
        /// The rooted seam's **valve declined** the collection.
        ValveDeclined,
        /// The rooted seam collected but the post-collection bump still did not fit.
        PostCollection,
        /// The rooted seam's publish/collect/scatter refused.
        Other,
    }

    impl FallbackSite {
        /// Every site, for a fixed-order report.
        pub(crate) const ALL: [FallbackSite; 5] = [
            FallbackSite::UnrootedSeam,
            FallbackSite::NoCallSite,
            FallbackSite::ValveDeclined,
            FallbackSite::PostCollection,
            FallbackSite::Other,
        ];

        /// This site's slot in [`ValveCounters::fallbacks_by_site`].
        fn index(self) -> usize {
            match self {
                FallbackSite::UnrootedSeam => 0,
                FallbackSite::NoCallSite => 1,
                FallbackSite::ValveDeclined => 2,
                FallbackSite::PostCollection => 3,
                FallbackSite::Other => 4,
            }
        }
    }

    /// Which of the machine's three root slices an offending word was found in.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum RootSlice {
        /// The x-register file (`RootSet::x`).
        X,
        /// The Erlang stack (`RootSet::stack`).
        Stack,
        /// The float-register file (`RootSet::fr`).
        Fr,
    }

    /// The **first** decline's offending root word, recorded once per measurement.
    ///
    /// It separates the two possible shapes the valve's machine-root decline conflates: a word in
    /// a **live** slot the engine really holds, and one in a slot past the declared live prefix —
    /// a stale register the engine has already finished with. `live_len` is the count the *same*
    /// call declared live, so `index < live_len` is a live register and `index >= live_len` is a
    /// dead one (for the stack slice, which has no live prefix, every slot counts as live).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct FirstDecline {
        /// The door the decline happened at.
        pub(crate) door: Door,
        /// The root slice the word came from.
        pub(crate) slice: RootSlice,
        /// The word's index within that slice.
        pub(crate) index: usize,
        /// The x-register count the same call declared live.
        pub(crate) live_len: usize,
    }

    /// Every door's tallies plus the runtime-wide witnesses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct ValveCounters {
        /// The narrow admission point ([`super::ensure_heap`]).
        pub(crate) ensure_heap: DoorCounts,
        /// The engine's full-root admission point ([`super::ensure_heap_full`]).
        pub(crate) ensure_heap_full: DoorCounts,
        /// The engine's rooted allocation seam ([`super::alloc_words_rooted`]).
        pub(crate) alloc_words_rooted: DoorCounts,
        /// The runner's park arm (`Runtime::serve_gc_request`).
        pub(crate) serve_gc_request: DoorCounts,
        /// Arena fallbacks ([`Runtime::note_gc_fallback`](crate::runtime::gc)) — the allocations
        /// the collector did not serve, each of which also **opens** the pid's descent latch.
        /// This is the authoritative total: it counts every call site, including the ones outside
        /// this module (`src/context/calls.rs`).
        pub(crate) fallbacks: usize,
        /// The same fallbacks, attributed to the **in-lane** refusal that caused them
        /// ([`FallbackSite`]). The two tally only where a site can name itself, so
        /// `fallbacks_at(..)` summed over every site is **at most** [`Self::fallbacks`].
        pub(crate) fallbacks_by_site: [usize; 5],
        /// The first decline's offending machine root word ([`FirstDecline`]), when the door that
        /// declined was one that hands its slices over. `None` means no door recorded one.
        pub(crate) first_decline: Option<FirstDecline>,
    }

    impl ValveCounters {
        /// The all-zero tally.
        const ZERO: ValveCounters = ValveCounters {
            ensure_heap: DoorCounts::ZERO,
            ensure_heap_full: DoorCounts::ZERO,
            alloc_words_rooted: DoorCounts::ZERO,
            serve_gc_request: DoorCounts::ZERO,
            fallbacks: 0,
            fallbacks_by_site: [0; 5],
            first_decline: None,
        };

        /// How many of the attributed fallbacks came from `site`.
        pub(crate) fn fallbacks_at(&self, site: FallbackSite) -> usize {
            self.fallbacks_by_site[site.index()]
        }
        /// This door's tallies, mutably.
        fn door_mut(&mut self, door: Door) -> &mut DoorCounts {
            match door {
                Door::EnsureHeap => &mut self.ensure_heap,
                Door::EnsureHeapFull => &mut self.ensure_heap_full,
                Door::AllocWordsRooted => &mut self.alloc_words_rooted,
                Door::ServeGcRequest => &mut self.serve_gc_request,
            }
        }

        /// Every door's tallies, in a fixed order.
        pub(crate) fn doors(&self) -> [(Door, DoorCounts); 4] {
            [
                (Door::EnsureHeap, self.ensure_heap),
                (Door::EnsureHeapFull, self.ensure_heap_full),
                (Door::AllocWordsRooted, self.alloc_words_rooted),
                (Door::ServeGcRequest, self.serve_gc_request),
            ]
        }
    }

    thread_local! {
        /// This thread's tallies since the last [`reset`].
        static COUNTERS: Cell<ValveCounters> =
            const { Cell::new(ValveCounters::ZERO) };
    }

    /// This thread's tallies since the last [`reset`].
    pub(crate) fn snapshot() -> ValveCounters {
        COUNTERS.with(Cell::get)
    }

    /// Zeroes this thread's tallies (a measurement's "before" reading).
    pub(crate) fn reset() {
        COUNTERS.with(|counters| counters.set(ValveCounters::ZERO));
    }

    /// Records that `door` reached its valve call, with the verdict that got it there and the
    /// number of machine root words the call is about to classify.
    pub(crate) fn note_reached(
        door: Door,
        occupancy: bool,
        debt: bool,
        forced: bool,
        machine_words: usize,
    ) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            let tally = value.door_mut(door);
            tally.reached += 1;
            tally.occupancy += usize::from(occupancy);
            tally.debt += usize::from(debt);
            tally.forced += usize::from(forced);
            tally.machine_words += machine_words;
            counters.set(value);
        });
    }

    /// Records one valve call's **root-word bound**: the words the caller *declared* and the words
    /// the door actually **handed over** (`published_words`). Called by the two full-root doors
    /// next to [`note_reached`], once per reach, so a test can pin the bound as a **structure** —
    /// the words the bound omits are exactly the declared words minus the published ones — rather
    /// than as a wall clock (see [`DoorCounts::published_words`]).
    pub(crate) fn note_root_words(door: Door, declared_words: usize, published_words: usize) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            let tally = value.door_mut(door);
            tally.declared_words += declared_words;
            tally.published_words += published_words;
            counters.set(value);
        });
    }

    /// Records one valve answer: whether the descent latch was open and whether the valve
    /// declined.
    pub(crate) fn note_valve(door: Door, descend: bool, declined: bool) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            let tally = value.door_mut(door);
            tally.latch_open += usize::from(descend);
            tally.hits += usize::from(declined);
            tally.descents += usize::from(descend && !declined);
            counters.set(value);
        });
    }

    /// Where the unscanned container a decline was caused by was found.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum DeclineCause {
        /// A machine root word (phase 1 over the caller's x/stack/FR slices).
        MachineRoot,
        /// A process root word (phase 1 over `Walk::seed_process_roots`).
        ProcessRoot,
        /// Only the descent (a nested shape).
        Descent,
    }

    /// Classifies a decline the valve already reported: **which root family** held the unscanned
    /// container the decline is attributable to?
    ///
    /// This is a test-only *re-derivation* of the walk's own phases, in the same order, so the
    /// attribution cannot disagree with the decline itself: the machine roots are classified
    /// first (the walk seeds them first and returns on the first hit), then the process roots
    /// alone with `descend == false` (which is exactly `Walk::seed_process_roots`'s per-word
    /// phase), and everything the descent had to find falls through to [`DeclineCause::Descent`].
    /// It costs one extra phase 1 in a test build; a release build has no counters at all.
    pub(crate) fn classify_decline<'a, I>(
        scratch: &mut super::super::gc_walk::WalkScratch,
        process: &beam_process::Process,
        machine_roots: I,
    ) -> DeclineCause
    where
        I: IntoIterator<Item = &'a Eterm>,
    {
        if machine_roots
            .into_iter()
            .any(|&word| super::root_is_unscanned(process, word))
        {
            return DeclineCause::MachineRoot;
        }
        if super::super::gc_walk::process_roots_are_unscanned(scratch, process, false) {
            return DeclineCause::ProcessRoot;
        }
        DeclineCause::Descent
    }

    /// Records which root family caused one decline (see [`classify_decline`]).
    pub(crate) fn note_decline(door: Door, cause: DeclineCause) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            let tally = value.door_mut(door);
            match cause {
                DeclineCause::MachineRoot => tally.machine_root_hits += 1,
                DeclineCause::ProcessRoot => tally.process_root_hits += 1,
                DeclineCause::Descent => tally.descent_hits += 1,
            }
            counters.set(value);
        });
    }

    /// Records the **first** decline's offending machine root word ([`FirstDecline`]) — the
    /// slice it came from, its index within that slice and the x-register count the same call
    /// declared live. Called by the doors that hold their slices as slices; a one-shot, so the
    /// extra scan costs one phase 1 in a test build and nothing in a release build.
    pub(crate) fn note_first_decline(
        door: Door,
        x: &[Eterm],
        stack: &[Eterm],
        fr: &[Eterm],
        live_len: usize,
        process: &beam_process::Process,
    ) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            if value.first_decline.is_some() {
                return;
            }
            let found = [(RootSlice::X, x), (RootSlice::Stack, stack), (RootSlice::Fr, fr)]
                .into_iter()
                .find_map(|(slice, words)| {
                    words
                        .iter()
                        .position(|&word| super::root_is_unscanned(process, word))
                        .map(|index| FirstDecline {
                            door,
                            slice,
                            index,
                            live_len,
                        })
                });
            if let Some(first) = found {
                value.first_decline = Some(first);
                counters.set(value);
            }
        });
    }

    /// Records that `door` ran a real collection.
    pub(crate) fn note_collected(door: Door) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.door_mut(door).collections += 1;
            counters.set(value);
        });
    }

    /// Records one arena fallback (`Runtime::note_gc_fallback`) and, with it, one latch
    /// **opening** event.
    ///
    /// This is the total, called from `Runtime::note_gc_fallback` itself, so it counts every
    /// caller — including the out-of-lane ones — and is the figure `Runtime::gc_stats().fallbacks`
    /// reports. [`note_fallback_at`] adds the in-lane attribution alongside it.
    pub(crate) fn note_fallback() {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.fallbacks += 1;
            counters.set(value);
        });
    }

    /// Attributes one arena fallback to the **in-lane refusal** that caused it
    /// ([`FallbackSite`]) — called next to `Runtime::note_gc_fallback` at the sites in this
    /// module that can name one.
    pub(crate) fn note_fallback_at(site: FallbackSite) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.fallbacks_by_site[site.index()] += 1;
            counters.set(value);
        });
    }
}

/// **Test-only** deterministic tallies for [`pointer_prefix_len`]'s block sweep.
///
/// The sweep's cost is a question about two structural facts — **how many whole blocks it skipped
/// without resolving their words individually** and **how many words the per-word fallback had to
/// examine** — never about timings. These counters answer exactly those questions, so a test can
/// pin the sweep's shape (a fallback of at most one block, block-bounded whenever the fast path
/// stopped on a block) instead of a wall clock.
///
/// They follow the same convention as [`valve_counters`]: a `#[cfg(test)] thread_local!` `Cell`
/// with a reset/read accessor, so the tallies exist **only in test builds** (a release binary pays
/// nothing) and a test reads its own thread's tally between a reset and the load it measures.
#[cfg(test)]
pub(crate) mod prefix_counters {
    use std::cell::Cell;

    /// One thread's tallies since the last [`reset`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct PrefixCounts {
        /// Calls to [`super::pointer_prefix_len`].
        pub(crate) calls: usize,
        /// Whole [`super::POINTER_PREFIX_BLOCK`]-word blocks the fast path skipped: their eight
        /// words were ANDed together and no one of them was resolved on its own.
        pub(crate) blocks_skipped: usize,
        /// Words the per-word fallback `rposition` examined, summed over every call — the
        /// [`super::POINTER_PREFIX_BLOCK`]-word block that stopped the fast path, or the
        /// sub-block tail left when every whole block was skipped.
        pub(crate) fallback_words: usize,
        /// Calls whose fallback was **block-bounded**: the fast path stopped on a block, so the
        /// scan covered that one block plus its offset.
        pub(crate) block_bounded: usize,
        /// Calls whose fallback was **tail-bounded**: every whole block was skipped, so only the
        /// `0..POINTER_PREFIX_BLOCK`-word tail was scanned.
        pub(crate) tail_bounded: usize,
    }

    impl PrefixCounts {
        /// The all-zero tally (the `const` form the thread-local's initialiser needs).
        pub(crate) const ZERO: PrefixCounts = PrefixCounts {
            calls: 0,
            blocks_skipped: 0,
            fallback_words: 0,
            block_bounded: 0,
            tail_bounded: 0,
        };
    }

    thread_local! {
        /// This thread's tallies since the last [`reset`].
        static COUNTERS: Cell<PrefixCounts> = const { Cell::new(PrefixCounts::ZERO) };
    }

    /// This thread's tallies since the last [`reset`].
    pub(crate) fn snapshot() -> PrefixCounts {
        COUNTERS.with(Cell::get)
    }

    /// Zeroes this thread's tallies (a measurement's "before" reading).
    pub(crate) fn reset() {
        COUNTERS.with(|counters| counters.set(PrefixCounts::ZERO));
    }

    /// Records one [`super::pointer_prefix_len`] call.
    pub(crate) fn note_call() {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.calls += 1;
            counters.set(value);
        });
    }

    /// Records one whole block the fast path skipped as all-immediate.
    pub(crate) fn note_block_skipped() {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.blocks_skipped += 1;
            counters.set(value);
        });
    }

    /// Records a fallback bounded to the block that stopped the fast path.
    pub(crate) fn note_block_bounded_fallback() {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.block_bounded += 1;
            value.fallback_words += super::POINTER_PREFIX_BLOCK;
            counters.set(value);
        });
    }

    /// Records a fallback bounded to a `words`-word sub-block tail.
    pub(crate) fn note_tail_bounded_fallback(words: usize) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.tail_bounded += 1;
            value.fallback_words += words;
            counters.set(value);
        });
    }
}
#[cfg(test)]
mod refs_plateau;
#[cfg(test)]
mod valve_bounds;
#[cfg(test)]
mod tests {
    //! Unit tests for the mechanical pieces of the adoption: the float guard scan, the safe
    //! heap bump, and the unrooted seam's grow-and-retry (which needs no `.beam` and so is
    //! asserted here). The collection itself is exercised end to end by `tests/gc_collect.rs`
    //! (it needs a real `.beam`, the engine and the scheduler).

    use beam_alloc::Heap;
    use beam_loader::load_file;
    use beam_term::prelude::{header_arity, is_small, make_small, NIL};
    use super::*;

    /// This crate's float fixture (`tests/fixtures/gc_floats.beam`): float arithmetic across
    /// a heap boundary.
    const FLOAT_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/gc_floats.beam"
    );
    /// The int-only control (`tests/fixtures/gc_ints.beam`).
    const INT_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/gc_ints.beam"
    );

    #[test]
    fn a_float_module_is_recognised_and_an_integer_module_is_not() {
        let floats = load_file(FLOAT_BEAM).expect("load the float fixture");
        assert!(
            module_uses_floats(&floats),
            "the fixture does float arithmetic across a test_heap boundary"
        );
        let ints = load_file(INT_BEAM).expect("load the integer fixture");
        assert!(
            !module_uses_floats(&ints),
            "the integer fixture has no float-family instruction"
        );
    }

    #[test]
    // The crate is `#![deny(unsafe_code)]`; this is a test-only raw read of words the bump
    // just handed out (the same exception the other crates' test modules take).
    #[allow(unsafe_code)]
    fn the_bump_advances_the_heap_by_exactly_n_words_and_returns_the_block_base() {
        let mut heap = Heap::new(64).expect("a 64-word heap");
        let before = heap.used();
        let base = heap.hp();
        let fill = [NIL; 8];
        let block = bump(&mut heap, 9, &fill).expect("nine words fit");
        assert_eq!(block, base, "the block starts where the heap ended");
        assert_eq!(heap.used(), before + 9, "exactly n words were consumed");
        // SAFETY: the bump just handed out nine words at `block`; the first is the header it
        // wrote and the callers of this function own the rest.
        unsafe {
            assert!(!is_small(*block), "the block base holds a written header");
            assert_eq!(header_arity(*block), 8, "an arityval header for n - 1 items");
        }
        // The element words are the placeholders: immediates the collector never relocates.
        // SAFETY: words 1..9 are inside the block.
        unsafe {
            assert_eq!(*block.add(1), NIL);
            assert_eq!(*block.add(8), NIL);
            // `NIL` is the atom `[]`, an immediate the collector never relocates — it is
            // neither a heap reference (`is_boxed`) nor a cons cell (`is_list`).
            assert!(!beam_term::prelude::is_boxed(*block.add(8)));
            assert!(!beam_term::prelude::is_list(*block.add(8)));
        }
    }

    #[test]
    fn a_zero_word_bump_is_a_no_op_and_a_bump_past_the_capacity_is_an_error() {
        let mut heap = Heap::new(4).expect("a four-word heap");
        let base = heap.hp();
        assert_eq!(bump(&mut heap, 0, &[]).expect("no words"), base);
        assert_eq!(heap.used(), 0, "a zero-word bump consumes nothing");
        let fill = [NIL; 8];
        assert!(bump(&mut heap, 9, &fill).is_err(), "nine words do not fit");
        assert_eq!(heap.used(), 0, "a failed bump leaves the heap untouched");
    }

    #[test]
    // The crate is `#![deny(unsafe_code)]`; this is a test-only raw read of the placeholder
    // words the bump just wrote.
    #[allow(unsafe_code)]
    fn a_bump_writes_only_immediates_after_its_header_word() {
        // The placeholder fill is immediates only, so a block the engine has not written yet
        // is inert to the collector: its header word is the one written value, and every
        // element word is an immediate (never a heap reference to relocate).
        let mut heap = Heap::new(16).expect("a 16-word heap");
        let fill = [make_small(7); 4];
        let block = bump(&mut heap, 5, &fill).expect("five words fit");
        for index in 1..5 {
            // SAFETY: the block owns five words and every one of them was just written.
            let word = unsafe { *block.add(index) };
            assert!(is_small(word), "placeholder word {index} is an immediate");
            assert!(!beam_term::prelude::is_boxed(word));
            assert!(!beam_term::prelude::is_list(word));
        }
    }

    /// **The unrooted allocation seam grows the nursery instead of permanently falling back to
    /// the arena.**
    ///
    /// `alloc_words` is the engine's metadata-only `ContextSink` path: it is handed **no** root
    /// set, so it may not collect. A full young heap is therefore met by
    /// [`Process::grow_nursery`] — the no-relocation growth seam (it retires the space it
    /// replaces and moves no live term) — and the bump is retried; only a growth that cannot
    /// reach the request falls through to the pointer-stable arena. Before that step a full
    /// nursery sent **every** subsequent block to the runtime-wide arena forever, so a churn
    /// loop's peak memory grew without bound.
    ///
    /// The loop asserts the three observable consequences directly on the seam: every block is
    /// served from a **nursery space** (never the arena), the nursery **grows** (its chain gains
    /// a space), and the runtime records **no** arena fallback.
    #[test]
    fn alloc_words_grows_the_nursery_and_never_falls_back_to_the_arena() {
        // The context shape of a scheduled slice: a real runtime plus the process table the pid
        // belongs to (two separate values, exactly as the runner's `take_processes` split — the
        // runtime does not own this table here).
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table
            .spawn_default()
            .expect("spawn a process in the pass table");

        let initial_spaces = table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count();
        assert_eq!(
            initial_spaces, 1,
            "a fresh process has a single nursery space"
        );

        // Each block consumes BLOCK_WORDS of the (233-word) initial nursery, so a loop of ROUNDS
        // calls exhausts the current space repeatedly and must grow the nursery each time.
        const BLOCK_WORDS: usize = 8;
        const ROUNDS: usize = 1_000;
        for round in 0..ROUNDS {
            let block = {
                let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
                alloc_words(&mut ctx, BLOCK_WORDS).expect("the block is served")
            };
            assert!(
                table
                    .get(pid)
                    .expect("the process is still in the table")
                    .collector()
                    .young_contains(block as *const Eterm),
                "round {round}: the block must be served from a nursery space, never the arena"
            );
        }

        let grown_spaces = table
            .get(pid)
            .expect("the process is still in the table")
            .collector()
            .young_space_count();
        assert!(
            grown_spaces > initial_spaces,
            "the loop must have grown the nursery ({initial_spaces} -> {grown_spaces} spaces)"
        );
        assert_eq!(
            rt.gc_stats().fallbacks,
            0,
            "no allocation may fall back to the pointer-stable arena once growth is available"
        );
    }

    /// The x-prefix and FR-prefix lengths a full-root door's hand-off left in `pid`'s root set:
    /// the registers' live x count, and the words on the process stack **beyond** `stack_words`
    /// (the FR words ride there, after the stack — see [`publish_roots_full`]).
    fn published_register_prefix(
        table: &ProcessTable,
        pid: beam_process::ProcessId,
        stack_words: usize,
    ) -> (usize, usize) {
        let registers = table.get(pid).expect("the process").registers();
        (
            registers.x().live(),
            registers.stack().len().saturating_sub(stack_words),
        )
    }

    /// **The full-root doors bound each register file to its pointer-bearing prefix.**
    ///
    /// The engine always hands over its **whole** register files (`Machine::x` and `Machine::fr`
    /// are each 1024 slots), and in a real workload they are overwhelmingly `NIL`. A copying
    /// collector acts on a root word only if it is a heap pointer, and [`root_is_unscanned`]
    /// answers `false` for every word that is neither boxed nor a list — so the door may bound each
    /// file to one past its last boxed-or-list word and only skip work: a word past the bound is an
    /// immediate, which no collection relocates and no reachability test reads.
    ///
    /// This pins the bound as a **structure** — the door's own root-word counters plus the root set
    /// the hand-off leaves in the process — never as a wall clock:
    ///
    /// 1. an all-immediate x/FR file publishes **zero** words from either file;
    /// 2. a boxed word at a **high** index (slot 1000) is included, so the bound is
    ///    "last pointer-bearing index + 1" and never a low-water guess;
    /// 3. the published word count is exactly that bound plus the whole stack — the declared
    ///    (whole-file) count is what the door is *handed*, and the difference between the two
    ///    counters is exactly the immediate tail the bound omits.
    #[test]
    fn the_full_root_doors_publish_only_the_pointer_bearing_prefix_of_each_register_file() {
        // The rig the other inline cases build: a real runtime plus the pass's own process table
        // (two separate values, as in the runner's `take_processes` split) and a process in it.
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("spawn a process");

        // The engine's own shape: a 1024-slot x file, a 1024-slot FR file, a short stack.
        const FILE: usize = 1024;
        let mut x = vec![NIL; FILE];
        let mut fr = vec![NIL; FILE];
        let mut stack = vec![NIL; 3];
        let stack_words = stack.len();

        // (1) Every word of both files is an immediate: the door is handed ~2 Ki words and
        //     publishes only the stack.
        valve_counters::reset();
        rt.request_gc(pid);
        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.current_module = Some("gc_ints".to_string());
            ensure_heap_full(
                &mut ctx,
                0,
                RootSet::new(&mut x[..], &mut stack[..], &mut fr[..], 1),
                true,
            )
            .expect("the heap-admission point never fails the program");
        }
        let immediates = valve_counters::snapshot().ensure_heap_full;
        assert_eq!(
            immediates.reached, 1,
            "the door's single valve call; {immediates:?}"
        );
        assert_eq!(
            immediates.declared_words,
            FILE + stack_words + FILE,
            "the door is handed both whole register files; {immediates:?}"
        );
        assert_eq!(
            immediates.published_words, stack_words,
            "an all-immediate x/FR file publishes nothing: only the stack is a root; \
             {immediates:?}"
        );
        assert_eq!(
            published_register_prefix(&table, pid, stack_words),
            (0, 0),
            "no x and no FR word may reach the process root set from an all-immediate file"
        );

        // (2) + (3) A pointer at a **high** index. The two boxed terms live on the process's own
        //     young heap (built after the collection above emptied it), and they sit at x[1000]
        //     and fr[5], so the bound is 1001 x words and 6 FR words.
        let high = table
            .get_mut(pid)
            .expect("the process")
            .heap_mut()
            .tuple(&[make_small(11), make_small(12)])
            .expect("the nursery builds {11,12}");
        let fifth = table
            .get_mut(pid)
            .expect("the process")
            .heap_mut()
            .tuple(&[make_small(13), make_small(14)])
            .expect("the nursery builds {13,14}");
        assert!(
            beam_term::prelude::is_boxed(high) && beam_term::prelude::is_boxed(fifth),
            "both terms are boxed heap pointers, so both are pointer-bearing roots"
        );
        x[1_000] = high;
        fr[5] = fifth;

        valve_counters::reset();
        rt.request_gc(pid);
        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.current_module = Some("gc_ints".to_string());
            ensure_heap_full(
                &mut ctx,
                0,
                RootSet::new(&mut x[..], &mut stack[..], &mut fr[..], 1),
                true,
            )
            .expect("the heap-admission point never fails the program");
        }
        let bounded = valve_counters::snapshot().ensure_heap_full;
        assert_eq!(
            bounded.reached, 1,
            "the door's single valve call; {bounded:?}"
        );
        assert_eq!(bounded.collections, 1, "the collection really ran; {bounded:?}");
        assert_eq!(
            bounded.declared_words,
            FILE + stack_words + FILE,
            "the declared count is unchanged by the bound; {bounded:?}"
        );
        assert_eq!(
            bounded.published_words,
            1_001 + stack_words + 6,
            "the bound is last-pointer-index + 1 on each file (x[1000] -> 1001, fr[5] -> 6) plus \
             the whole stack; {bounded:?}"
        );
        let (published_x, published_fr) = published_register_prefix(&table, pid, stack_words);
        assert_eq!(
            (published_x, published_fr),
            (1_001, 6),
            "the hand-off publishes exactly the bound: a high x slot is never dropped"
        );
        assert_eq!(
            bounded.published_words,
            published_x + stack_words + published_fr,
            "the counter is the hand-off's own region: x prefix + whole stack + FR prefix"
        );
        assert!(
            bounded.declared_words - bounded.published_words == (FILE - 1_001) + (FILE - 6),
            "what the bound omits is exactly the immediate tail of each file: \
             declared={} published={}",
            bounded.declared_words,
            bounded.published_words
        );
    }

    /// **The block sweep is an optimization only: `pointer_prefix_len` returns the same bound
    /// whether it skips an immediate run a block at a time or walks it word by word — and the
    /// word-by-word fallback never scans more than one block.**
    ///
    /// A block is skipped only when the AND of its words still carries the `IMMED1` primary
    /// tag, which can happen only if *every* word of the block carries it; the block that stops
    /// the sweep then holds the file's last boxed-or-list word, so the fallback is bounded to it.
    /// Every case below is pinned twice — against an explicit expected index, and against the
    /// per-word scan (the independent reference implementation) — **and** against the sweep's own
    /// counters, which pin that fallback's width. The 8-word-aligned families (exhaustive over one
    /// block's tag patterns, then exhaustive over the natural indices, then pseudo-random) cover
    /// the two ends of the sweep: a pointer at index 0, in the middle, at the start of the last
    /// block and in the very last word of the file, where only the last block carries one. The
    /// mixed pseudo-random family over all four primary tags covers files of every length.
    #[test]
    fn the_block_skip_finds_the_same_pointer_prefix_as_the_per_word_scan() {
        use beam_term::prelude::{is_boxed, is_immediate, is_list};

        const BLOCK: usize = POINTER_PREFIX_BLOCK;
        // `NIL`: the filler of an unwritten register slot, and the shape a file's tail is.
        const IMM: Eterm = NIL;
        // A `BOXED`-tagged word (primary tag 2) and a `LIST`-tagged one (tag 1): the two
        // pointer-bearing shapes.
        const BOXED: Eterm = Eterm::from_raw(0x2);
        const LIST: Eterm = Eterm::from_raw(0x1);
        // A bare header word (primary tag 0, value 0 — the C ABI's `THE_NON_VALUE`): not an
        // immediate, and both mirrored C bit tricks accept it (`is_boxed` is `(x & 0x1) == 0`,
        // `is_list` is `(x & 0x2) == 0`), so the sweep must stop at the block holding it and the
        // per-word fallback must resolve it exactly as before — a header word is not an immediate
        // run, so it may not be skipped past.
        const HEADER: Eterm = Eterm::from_raw(0);

        assert!(is_immediate(IMM) && !is_boxed(IMM) && !is_list(IMM));
        assert!(!is_immediate(BOXED) && is_boxed(BOXED) && !is_list(BOXED));
        assert!(!is_immediate(LIST) && !is_boxed(LIST) && is_list(LIST));
        assert!(!is_immediate(HEADER) && is_boxed(HEADER) && is_list(HEADER));

        // The rule the block sweep must not change: one past the last boxed-or-list word.
        fn per_word(words: &[Eterm]) -> usize {
            words
                .iter()
                .rposition(|&word| {
                    beam_term::prelude::is_boxed(word) || beam_term::prelude::is_list(word)
                })
                .map_or(0, |index| index + 1)
        }

        // One call, with the sweep's **structure** pinned by its counters: exactly one fallback
        // scan, never wider than one block — and exactly one block, scanned from the start of
        // that block, whenever the fast path stopped on one.
        fn checked(file: &[Eterm]) -> usize {
            prefix_counters::reset();
            let bound = pointer_prefix_len(file);
            let counts = prefix_counters::snapshot();
            assert_eq!(counts.calls, 1, "one sweep per call; {counts:?}");
            assert_eq!(
                counts.block_bounded + counts.tail_bounded,
                1,
                "every call ends in exactly one fallback scan; {counts:?}"
            );
            if counts.block_bounded == 1 {
                assert_eq!(
                    counts.fallback_words, BLOCK,
                    "the fast path stopped on a block, so the fallback scanned exactly that one \
                     block and no more; {counts:?}"
                );
                assert!(
                    (counts.blocks_skipped + 1) * BLOCK <= file.len(),
                    "the block that stopped the sweep lies inside the file; {counts:?}"
                );
            } else {
                assert_eq!(
                    counts.fallback_words,
                    file.len() % BLOCK,
                    "no block stopped the sweep, so only the sub-block tail was scanned; \
                     {counts:?}"
                );
                assert_eq!(
                    counts.blocks_skipped,
                    file.len() / BLOCK,
                    "every whole block was skipped; {counts:?}"
                );
            }
            bound
        }

        // An empty file, and every all-immediate file: nothing to publish — shorter than one
        // block, exactly one block, and past several.
        let empty: Vec<Eterm> = Vec::new();
        assert_eq!(checked(&empty), 0, "an empty file has no pointer word");
        for len in 0..=(2 * BLOCK + 1) {
            let file = vec![IMM; len];
            assert_eq!(
                checked(&file),
                0,
                "an all-immediate file of {len} words publishes nothing"
            );
            assert_eq!(checked(&file), per_word(&file));
        }

        // The only pointer-bearing word at the first index, the last, and the middle — for a
        // file shorter than one block, exactly one block, and a multi-block file, so the
        // pointer lands on a block boundary, inside a block and at the very end.
        for len in [1, 2, BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK, 3 * BLOCK] {
            for index in [0, len - 1, len / 2] {
                for pointer in [BOXED, LIST] {
                    let mut file = vec![IMM; len];
                    file[index] = pointer;
                    assert_eq!(
                        checked(&file),
                        index + 1,
                        "the only pointer-bearing word is at {index} of {len}"
                    );
                    assert_eq!(checked(&file), per_word(&file));
                }
            }
        }

        // The 8-word-aligned family, exhaustively: a whole-block file whose **last** block carries
        // every one of the 4^8 tag patterns (the two leading blocks stay all-immediate, so the
        // sweep must skip them and stop on the last one) — except the all-zero pattern, where it
        // skips all three and falls back to the empty tail. A pattern with a pointer in the
        // block's first slot and one with a pointer only in the file's very last word are both in
        // here, which is exactly the pair of ends the bounded fallback exists for.
        for pattern in 0..(1usize << (2 * BLOCK)) {
            let mut file = vec![IMM; 3 * BLOCK];
            for index in 0..BLOCK {
                file[2 * BLOCK + index] = match (pattern >> (2 * index)) & 0x3 {
                    0 => IMM,
                    1 => BOXED,
                    2 => LIST,
                    _ => HEADER,
                };
            }
            let expected = per_word(&file);
            assert_eq!(
                checked(&file),
                expected,
                "tag pattern {pattern:#06x} of the last block changed the bound"
            );
        }

        // The same, one pointer at a time and on whole-block files only: index 0, the middle, the
        // start of the last block, and the very last word — the last two are the cases where only
        // the final block carries a pointer.
        for blocks in 1..=4usize {
            let len = blocks * BLOCK;
            for index in [0, len / 2, len - BLOCK, len - 1] {
                for pointer in [BOXED, LIST] {
                    let mut file = vec![IMM; len];
                    file[index] = pointer;
                    assert_eq!(
                        checked(&file),
                        index + 1,
                        "the only pointer-bearing word is at {index} of {len}"
                    );
                    assert_eq!(checked(&file), per_word(&file));
                }
            }
        }

        // The **last** pointer word wins: the sweep stops at the block holding it and the
        // fallback resolves its exact index, so a list deep in a long immediate tail is never
        // dropped.
        let mut file = vec![IMM; 3 * BLOCK];
        file[BLOCK - 1] = BOXED;
        file[2 * BLOCK + 3] = LIST;
        assert_eq!(checked(&file), 2 * BLOCK + 4);
        assert_eq!(checked(&file), per_word(&file));

        // A header word is not an immediate, so its block is never skipped and the per-word
        // fallback resolves it exactly as before (both `is_boxed` and `is_list` accept the
        // value-invalid `HEADER` pattern).
        for index in [0, BLOCK, 2 * BLOCK, 2 * BLOCK + 5] {
            let header_at = |len: usize| {
                let mut file = vec![IMM; len];
                file[index] = HEADER;
                file
            };
            for len in [index + 1, 3 * BLOCK] {
                let file = header_at(len);
                assert_eq!(
                    checked(&file),
                    index + 1,
                    "a header word at {index} of {len} bounds the prefix"
                );
                assert_eq!(checked(&file), per_word(&file));
            }
        }

        // An immediate tail shorter than one block is resolved by the per-word fallback alone.
        assert_eq!(checked(&[BOXED, IMM, IMM]), 1);
        assert_eq!(checked(&[IMM, IMM, BOXED]), 3);
        assert_eq!(checked(&[IMM, BOXED, IMM]), 2);

        // Deterministic pseudo-random files over all four primary tags, in both shapes: an
        // arbitrary length, and a whole-block one.
        let tags = [IMM, BOXED, LIST, HEADER];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        for case in 0..4_096usize {
            let len = next() % (4 * BLOCK + 3);
            let mut file = Vec::with_capacity(len);
            for _ in 0..len {
                file.push(tags[next() % tags.len()]);
            }
            assert_eq!(
                checked(&file),
                per_word(&file),
                "case {case}: the block sweep changed the bound of {file:?}"
            );
        }
        for case in 0..4_096usize {
            let len = (next() % 4 + 1) * BLOCK;
            let mut file = Vec::with_capacity(len);
            for _ in 0..len {
                file.push(tags[next() % tags.len()]);
            }
            assert_eq!(
                checked(&file),
                per_word(&file),
                "aligned case {case}: the block sweep changed the bound of {file:?}"
            );
        }
    }

    /// A **foreign region** for the valve's tests: a heap block that is neither the process
    /// heap's live storage nor any space of its collector, plus the terms the forwarding test
    /// has to tell apart.
    ///
    /// The terms are written by the same heap-agnostic `beam_term` writers the production
    /// producers use ([`beam_term::boxed::write_internal_ref`] — the exact M1 block `make_ref/0`'s
    /// native floor writes, all immediates after its header — an ordinary tuple, a cons and an
    /// `ErlSubBits`), so the shapes are the real ones and not stand-ins.
    ///
    /// The fixture is split exactly the way the valve's second fact splits it: the plain terms
    /// forward only immediates (and a literal, for the sub-bits), and the `*_over` ones forward a
    /// word of the asking process's nursery.
    struct ForeignRegion {
        /// The block the terms live in; dropped last, after the terms are done with.
        heap: Heap,
        /// `make_ref/0`'s frozen M1 childless block (`_ => None` in the child-slot table).
        reference: Eterm,
        /// A 2-tuple of immediates: two child slots, neither relocated.
        tuple: Eterm,
        /// A cons cell over immediates: header-less, both words immediates.
        cons: Eterm,
        /// An `ErlSubBits` whose `orig` word is a **tagged literal** — the production shape of a
        /// bitstring spliced out of a literal binary, and a word no relocation touches.
        sub_bits: Eterm,
        /// A 2-tuple over a term of the asking process's nursery.
        tuple_over_young: Eterm,
        /// An `ErlSubBits` whose `orig` word is a term of the asking process's nursery.
        sub_bits_over_young: Eterm,
        /// A cons over a term of the asking process's nursery.
        cons_over_young: Eterm,
        /// The nursery term the `*_over_young` terms forward.
        young: Eterm,
    }

    impl ForeignRegion {
        /// Builds the terms in one fresh foreign block, plus a nursery term on `process`'s young
        /// heap for the `*_over_young` half to forward.
        #[allow(unsafe_code)]
        fn new(process: &mut Process) -> ForeignRegion {
            let child = process
                .heap_mut()
                .tuple(&[make_small(7), make_small(8)])
                .expect("a young-heap 2-tuple");
            let young = child;
            let mut heap = Heap::new(96).expect("a 96-word foreign block");
            let fill = [NIL; 8];
            // SAFETY: each `bump` reserves exactly the words the writer it feeds needs, inside
            // this block, and every word is written before the term escapes.
            unsafe {
                let slot = bump(&mut heap, 4, &fill).expect("four words for the reference");
                let reference = beam_term::boxed::write_internal_ref(slot, 7)
                    .expect("an M1 internal reference");
                let tuple = heap
                    .tuple(&[make_small(1), make_small(2)])
                    .expect("a 2-tuple");
                let slot = bump(&mut heap, 2, &fill).expect("two words for the cons");
                *slot = make_small(3);
                *slot.add(1) = NIL;
                let cons = beam_term::list::make_list(slot);
                let literal = beam_term::literal::make_literal(child);
                let slot = bump(&mut heap, 5, &fill).expect("five words for the ErlSubBits");
                let sub_bits = beam_term::bitstring::write_sub_bits(
                    slot,
                    make_small(0),
                    make_small(0),
                    make_small(0),
                    literal,
                );
                let tuple_over_young = heap
                    .tuple(&[child, make_small(4)])
                    .expect("a 2-tuple over the nursery term");
                let slot = bump(&mut heap, 5, &fill).expect("five words for the ErlSubBits");
                let sub_bits_over_young = beam_term::bitstring::write_sub_bits(
                    slot,
                    make_small(0),
                    make_small(0),
                    make_small(0),
                    child,
                );
                let slot = bump(&mut heap, 2, &fill).expect("two words for the cons");
                *slot = child;
                *slot.add(1) = NIL;
                let cons_over_young = beam_term::list::make_list(slot);
                ForeignRegion {
                    heap,
                    reference,
                    tuple,
                    cons,
                    sub_bits,
                    tuple_over_young,
                    sub_bits_over_young,
                    cons_over_young,
                    young,
                }
            }
        }

        /// Every term of the fixture with the verdict the valve must give it, in the fixture's
        /// own order: the inert group first, then the forwarding one.
        fn cases(&self) -> Vec<(&'static str, Eterm, bool)> {
            vec![
                ("reference (leaf)", self.reference, false),
                ("tuple of immediates", self.tuple, false),
                ("cons of immediates", self.cons, false),
                ("sub-bits over a literal", self.sub_bits, false),
                ("tuple over a young term", self.tuple_over_young, true),
                ("cons over a young term", self.cons_over_young, true),
                ("sub-bits over a young term", self.sub_bits_over_young, true),
            ]
        }

        /// Asserts every term really lives in this block and **not** in the process's heap.
        fn assert_foreign(&self, process: &Process) {
            let base = (self.heap.hp() as usize)
                .wrapping_sub(self.heap.used().saturating_mul(std::mem::size_of::<Eterm>()));
            for (label, term, _) in self.cases() {
                let ptr = beam_term::list::ptr_val(term) as usize;
                assert!(
                    ptr >= base && ptr < self.heap.hend() as usize,
                    "the {label} must live in the foreign block"
                );
                assert!(
                    !in_heap_block(process.heap(), ptr),
                    "the {label} must be outside the process heap's live storage"
                );
                assert!(
                    !process.collector().young_contains(ptr as *const Eterm)
                        && !process.collector().mature_contains(ptr as *const Eterm),
                    "the {label} must be outside every scanned space"
                );
            }
            assert!(
                in_heap_block(process.heap(), beam_term::list::ptr_val(self.young) as usize),
                "the fixture's nursery term must live in the process's mutator block"
            );
            assert!(
                word_is_scanned(process, &process.collector().scanned_bounds(), self.young),
                "the fixture's nursery term must be a word the collector relocates"
            );
        }
    }

    /// **A foreign container declines a collection only when it forwards a *scanned* word.**
    ///
    /// [`root_is_unscanned`] is a conjunction of two facts, and this is the fixture that pins the
    /// second one across every shape the production producers write: the term lives in a region
    /// the collector does not scan ([`ForeignRegion`] asserts that for every case), and only then
    /// does it matter whether it forwards anything a relocation would act on.
    ///
    /// The *inert* group must **not** decline. A copying collector's only obligations toward a
    /// container are to *follow* and *update* the words it forwards, so a shape whose cells are
    /// all immediates — or a tagged literal, which no relocation touches — imposes neither: it can
    /// neither dangle under a relocation nor keep a young word alive. `make_ref/0`'s block is the
    /// degenerate case ([`beam_term::layout::for_each_child_slot`] visits no slot at all, so an
    /// allocating process holds one permanently), and a pooled-BIF-heap tuple or an arena cons of
    /// small integers is the same obligation-free shape.
    ///
    /// The *forwarding* group must decline: each of its `*_over_young` terms really holds a word
    /// of the asking process's nursery, which a collection would relocate out from under a block
    /// it does not scan. `SUB_BITS_SUBTAG` is the boundary the canonical enumeration gets right
    /// for free — its `orig` word **is** forwarded, so an `ErlSubBits` over a live word declines
    /// while the same shape over a literal does not.
    #[test]
    fn a_foreign_leaf_is_not_unscanned_but_a_container_forwarding_a_young_word_is() {
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("spawn a process");
        let region = ForeignRegion::new(table.get_mut(pid).expect("the spawned process"));
        let process = table.get(pid).expect("the spawned process");
        region.assert_foreign(process);

        // Every shape, through the production predicate and through the reachability walk the
        // valve's descent (and the process table's collection gate) drive: the inert group
        // reaches **no hit**, and each forwarding container is a phase-1 hit.
        let mut scratch = crate::context::gc_walk::WalkScratch::default();
        for (label, term, expected) in region.cases() {
            assert_eq!(
                super::root_is_unscanned(process, term),
                expected,
                "the foreign {label} must be unscanned == {expected}"
            );
            assert_eq!(
                crate::context::gc_walk::any_root_is_unscanned(
                    &mut scratch,
                    process,
                    std::iter::once(&term),
                    true,
                ),
                expected,
                "the walk must classify the foreign {label} as unscanned == {expected}"
            );
        }
    }

    /// **Only a foreign container that forwards a scanned word opens the per-pid descent latch.**
    ///
    /// [`RuntimeContext::open_gate_for_foreign_result`] is the one production site that decides
    /// whether a registered-BIF/native-floor/NIF result must arm the
    /// [`Runtime::note_foreign_reachable`] bit — the bit that makes every candidate collection of
    /// the process also run the `O(live)` descent. A wrong `true` costs that descent on every
    /// admission point until a clean **full**-root descent closes it again
    /// ([`Runtime::close_foreign_reachable`]), which is why the seam must not arm it for a result
    /// that imposes no follow/update obligation. Driving the real seam (never a re-derivation of
    /// it) is what pins the answers.
    ///
    /// This seam can only ever *open* the latch: the clear belongs to the two full admission
    /// doors, which are the only callers that hand the valve a whole root set, so a later inert
    /// result here leaves an open latch open.
    #[test]
    fn only_a_foreign_container_forwarding_a_scanned_word_opens_the_descent_latch() {
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("spawn a process");
        let region = ForeignRegion::new(table.get_mut(pid).expect("the spawned process"));
        assert!(!rt.foreign_reachable(pid), "a fresh pid's latch is closed");

        // The inert group: a foreign result that forwards nothing a relocation acts on.
        for (label, term, expected) in region.cases() {
            if expected {
                continue;
            }
            {
                let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
                ctx.open_gate_for_foreign_result(term);
            }
            assert!(
                !rt.foreign_reachable(pid),
                "the inert foreign {label} must not open the descent latch"
            );
        }

        // ... and one container that forwards a nursery word does.
        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.open_gate_for_foreign_result(region.cons_over_young);
        }
        assert!(
            rt.foreign_reachable(pid),
            "a foreign cons over a nursery word must open the descent latch"
        );

        // The seam is open-only: closing belongs to the full-root doors, so a later inert result
        // can never take the latch back down here.
        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.open_gate_for_foreign_result(region.reference);
        }
        assert!(
            rt.foreign_reachable(pid),
            "the BIF-result seam only opens the latch; a clean result cannot close it"
        );
    }
}