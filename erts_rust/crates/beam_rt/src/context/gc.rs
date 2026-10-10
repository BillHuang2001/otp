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
//! the request to the arena. Everything outside a scheduled slice keeps the runtime's
//! pointer-stable arena, byte for byte as before — see [`crate::runtime::gc`] for the state,
//! the accounting and the safety valves.
//!
//! # The root set is the WHOLE [`RootSet`]
//!
//! The root set a collection relocates is the **executing machine's whole live root set** as
//! the engine hands it over: **every** slot of the X-register file, the **whole** Erlang
//! stack and the **whole** FR file ([`publish_roots_full`]/[`scatter_roots_full`]).
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
//! # The float guard
//!
//! A module whose code contains a float-family instruction is **never** collected over
//! ([`module_uses_floats`], cached per module name); everything else — most importantly a
//! floating-point *BIF* such as `math:sqrt/1`, whose code is in another module — is then
//! harmless, because that module's own code is never the code under a collection. An
//! unresolvable call site (a fun entry or the `-eval` evaluator, which have no
//! `current_module`) is not collected over either — conservative, never wrong.

use beam_alloc::Heap;
use beam_common::Result;
use beam_engine::context::RootSet;
use beam_gc::{Collection, CollectionKind};
use beam_loader::{Instr, Module};
use beam_process::{Process, ProcessTable, MAX_REG};
use beam_term::Eterm;

use crate::runtime::Runtime;
use crate::RuntimeContext;

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

/// Whether `term` is a **container living in a region the collector does not scan** — the
/// precondition that makes a collection of the owning process unsound.
///
/// A copying collector relocates exactly the words it finds in its *from* set (the young space,
/// plus the mature space for a major) and classifies every other address as
/// [`Target::Foreign`](beam_gc): it leaves such a pointer where it is **and does not follow it**
/// — so a young word a foreign container points *at* is neither kept alive by it nor updated
/// when it moves. A term with heap children is therefore only safe in a *scanned* region. The
/// unscanned regions this runtime really creates are the pooled BIF heaps
/// ([`Runtime::take_bif_heap`]), the `-eval` heap ([`Runtime::stabilize_term`]), the
/// pointer-stable arena ([`crate::context::gc::alloc_words`]'s fallback) and a
/// [`beam_message`] heap fragment.
///
/// The test is the conservative *complement* of the scanned set, so it needs no enumeration of
/// the open-ended unscanned side: a term is safe exactly when it is an **immediate** (neither
/// boxed nor a cons), a **literal** (a frozen code constant in the literal area, which never
/// points into a movable heap), or a pointer into the process's own **young** or **mature**
/// heap. Anything else — including a container handed back by a registered BIF or built on the
/// arena — declines the collection.
pub(crate) fn root_is_unscanned(process: &Process, term: Eterm) -> bool {
    if !beam_term::prelude::is_boxed(term) && !beam_term::prelude::is_list(term) {
        return false;
    }
    if beam_term::literal::is_literal(term) {
        return false;
    }
    let ptr = beam_term::list::ptr_val(term) as usize;
    !in_heap_block(process.heap(), ptr)
        && !in_heap_block(process.collector().mature().heap(), ptr)
}

/// Whether **any** of the given machine roots is a container in an unscanned region (see
/// [`root_is_unscanned`]).
pub(crate) fn any_root_is_unscanned<'a, I>(process: &Process, roots: I) -> bool
where
    I: IntoIterator<Item = &'a Eterm>,
{
    roots.into_iter().any(|word| root_is_unscanned(process, *word))
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
/// this one takes the engine's [`RootSet`] and publishes **every** slot of it:
///
/// * all of `roots.x` (the [`RootSet::live`] hint is deliberately **ignored** — a copying
///   collector may always over-approximate its root set, so treating every register as live
///   can only keep a term alive that would otherwise die, never free a live one, and the
///   engine has no way to prove a register dead);
/// * then every word of `roots.stack`; and
/// * then every word of `roots.fr`.
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
/// This is the publish implementation shared by the in-slice heap-admission point
/// ([`ensure_heap_full`]) and the runner's **park** arm, so a parked machine's FR file cannot
/// dangle either.
///
/// # Errors
///
/// None: an impossible `set_live` is reported as `false` (the caller's "do not collect").
pub(crate) fn publish_roots_full(process: &mut Process, roots: &RootSet<'_>) -> bool {
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
            for word in roots.fr.iter() {
                process_stack.push(*word);
            }
            process.registers().stack().len() == roots.stack.len() + roots.fr.len()
        }
        Err(_) => false,
    }
}

/// Write a collected process's (possibly relocated) roots back into the machine's slices —
/// the **scatter** half of the full-root hand-off.
///
/// The exact inverse of [`publish_roots_full`]: it requires the relocated x-register prefix
/// to be as long as [`publish_roots_full`] published and the relocated stack to hold exactly
/// the stack words **plus** the FR words, then splits that region back into `roots.stack` and
/// `roots.fr` in the order they were appended. The collector preserves the slice lengths and
/// the word order, so a length mismatch means the machine's slices can no longer be trusted:
/// the function reports `false` and the caller must stop collecting over this process rather
/// than write a mis-sized block back.
pub(crate) fn scatter_roots_full(process: &Process, roots: &mut RootSet<'_>) -> bool {
    let x_count = roots.x.len().min(MAX_REG);
    let regs = process.registers();
    let relocated_x = regs.x().live_slice();
    let relocated_stack = regs.stack().as_slice();
    if relocated_x.len() != x_count || relocated_stack.len() != roots.stack.len() + roots.fr.len()
    {
        return false;
    }
    roots.x[..x_count].copy_from_slice(relocated_x);
    let stack_words = roots.stack.len();
    roots.stack.copy_from_slice(&relocated_stack[..stack_words]);
    roots.fr.copy_from_slice(&relocated_stack[stack_words..]);
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
/// request is larger than the nursery — or when a `garbage_collect` BIF asked for one.
///
/// `armed` is the slice latch (see the module docs). When it is not set, when the running
/// process is not collectable, or when the call site's module is not known to be float-free,
/// this is a no-op — exactly the pre-collector runtime.
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
    // The float guard: only a call site whose module is known to be float-free may be
    // collected over. An unknown call site (a fun entry, the `-eval` evaluator) is skipped,
    // never guessed at.
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
    if !forced && !process.should_collect(heap_need as usize) {
        return Ok(());
    }
    // The **unscanned-root valve** (see [`root_is_unscanned`]): a live root in a region the
    // collector does not scan would be left pointing at young words this move relocates, so the
    // move is **declined** — the heap keeps its terms, the unscanned root stays valid and the
    // program stays correct. The test runs *after* the occupancy short-circuit (an ordinary
    // admission point that is not about to collect pays nothing) and *before* the request is
    // consumed, so a forced collection it refuses stays armed for the runner's park arm.
    if any_root_is_unscanned(process, x_regs.iter().chain(stack.iter())) {
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
/// live root set ([`RootSet`]: the x-register file, the whole Erlang stack **and the whole FR
/// file**).
///
/// This is the full-root twin of [`ensure_heap`], reached by
/// `Machine::ensure_heap`/`test_heap`/`allocate_heap` through
/// [`ExecutionContext::ensure_heap_full`](beam_engine::ExecutionContext::ensure_heap_full).
/// The **gate order is identical** to the narrow door's — the latch, the forced-collection
/// request, the collectability test, the float guard, the table/process lookup, the
/// occupancy test and the "committed" request consume — and only the root hand-off differs:
/// it publishes and scatters `roots` (every x slot, the whole stack, the whole FR file)
/// through [`publish_roots_full`]/[`scatter_roots_full`] instead of a `live`-bounded x prefix.
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
/// in the same run, so leaving it un-relocated makes the heap move out from under it.
///
/// # Errors
///
/// As [`ensure_heap`]: no failure mode reaches the program. A failed (or mis-sized)
/// collection halts collection for that process
/// ([`Runtime::halt_process_gc`](crate::runtime::gc)) and reports success.
pub(crate) fn ensure_heap_full(
    ctx: &mut RuntimeContext<'_>,
    heap_need: u32,
    mut roots: RootSet<'_>,
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
    let Some(name) = ctx.current_module.clone() else {
        return Ok(());
    };
    if !module_may_be_collected(ctx.runtime, &name) {
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
    if !forced && !process.should_collect(heap_need as usize) {
        return Ok(());
    }
    // The **unscanned-root valve** — the full-root twin of the one in [`ensure_heap`]: a root in
    // a region the collector does not scan declines the move (see [`root_is_unscanned`]), so the
    // heap keeps its terms and the unscanned root stays valid. Tested after the occupancy
    // short-circuit and before the request is consumed, so a refused forced collection stays
    // armed for the runner's park arm.
    if any_root_is_unscanned(
        process,
        roots
            .x
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr.iter()),
    ) {
        return Ok(());
    }
    if forced {
        runtime.take_gc_request(pid);
    }

    if !publish_roots_full(process, &roots) {
        runtime.halt_process_gc(pid);
        return Ok(());
    }

    // The action is the same as the narrow door's: collect, or grow the nursery to fit a
    // request the post-collection nursery cannot hold. See [`collect_or_grow`].
    let action = collect_or_grow(process, heap_need);
    match action {
        Ok(collection) => {
            runtime.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
        }
        Err(_) => {
            runtime.halt_process_gc(pid);
            return Ok(());
        }
    }

    if !scatter_roots_full(process, &mut roots) {
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
/// process, a process the table no longer holds, **and a collectable slice process whose own
/// young heap cannot serve the request** — keeps the pointer-stable arena, byte for byte as
/// before this seam existed. A fallback is never an error: the arena cannot fail where the
/// young heap could.
///
/// # Why the arena fallback stays for a collectable slice process
///
/// This seam is the **unrooted** allocation path (`beam_engine::terms::sink::ContextSink`,
/// metadata-only builds): it is handed **no** root set, so it can neither collect nor safely
/// grow — calling `Process::ensure_capacity` here would collect against the possibly-stale
/// published `process.regs`, relocating a heap out from under live machine terms (a wrong
/// result). The arena block it hands out is a container in an unscanned region, but that is
/// already made sound by the admission-point valve ([`root_is_unscanned`]: `ensure_heap`/
/// `ensure_heap_full` **decline** any collection whose root set reaches such a container), so
/// such a root stays valid and no young word it reaches is moved. Returning the young heap's
/// typed `Err` here instead would only kill a process (e.g. the boot chain) that the arena keeps
/// alive, and the engine reaches an admission point (`ensure_heap`) before it constructs terms.
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
                let fill = runtime.gc_scratch(n.saturating_sub(1));
                if let Ok(block) = bump(process.heap_mut(), n, fill) {
                    return Ok(block);
                }
            }
            runtime.note_gc_fallback();
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
/// * **The latch is set** (the slice's own machine): prefer the young heap exactly as
///   [`alloc_words`] does, and only when that bump **fails** run one collection over the
///   handed-in roots and retry the bump. A collection or scatter failure halts collection for
///   that process ([`Runtime::halt_process_gc`](crate::runtime::gc)) and falls back to the
///   arena. The float guard and the `current_module` requirement both apply, exactly as in
///   [`ensure_heap_full`], because a collection over a module that touches the machine's
///   float-register file could corrupt a live float register.
///
/// # Errors
///
/// Propagates the arena's allocation failure only — never a collection failure.
pub(crate) fn alloc_words_rooted(
    ctx: &mut RuntimeContext<'_>,
    n: usize,
    mut roots: RootSet<'_>,
    armed: bool,
) -> Result<*mut Eterm> {
    let pid = ctx.mailbox_pid;
    // Read the call site's module *before* the context is destructured: the float guard below
    // needs it, and it is only meaningful when this is the slice's own machine.
    let current_module = if armed {
        ctx.current_module.clone()
    } else {
        None
    };
    let RuntimeContext {
        runtime,
        mailbox_table,
        ..
    } = ctx;
    let Some(table) = mailbox_table else {
        return runtime.alloc_words(n);
    };
    if !runtime.gc_process_enabled(pid) {
        return runtime.alloc_words(n);
    }
    // The same attempt `alloc_words` makes: the running process's own young heap.
    if let Some(process) = table.get_mut(pid) {
        let fill = runtime.gc_scratch(n.saturating_sub(1));
        if let Ok(block) = bump(process.heap_mut(), n, fill) {
            return Ok(block);
        }
    }
    // The young heap refused. Only the slice's own machine may collect over the roots it
    // handed in; every other context keeps `alloc_words`' arena fallback, byte for byte.
    let Some(name) = current_module else {
        runtime.note_gc_fallback();
        return runtime.alloc_words(n);
    };
    if !module_may_be_collected(runtime, &name) {
        runtime.note_gc_fallback();
        return runtime.alloc_words(n);
    }
    let Some(process) = table.get_mut(pid) else {
        runtime.note_gc_fallback();
        return runtime.alloc_words(n);
    };
    // The unscanned-root valve (see [`root_is_unscanned`]): collecting over a root set that
    // contains a container in a region the collector does not scan would relocate the young
    // words it reaches without updating it, so this collection is declined and the block comes
    // from the pointer-stable arena — the same honest fallback every other refusal path takes.
    if any_root_is_unscanned(
        process,
        roots
            .x
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr.iter()),
    ) {
        runtime.note_gc_fallback();
        return runtime.alloc_words(n);
    }
    if !publish_roots_full(process, &roots) {
        runtime.halt_process_gc(pid);
        runtime.note_gc_fallback();
        return runtime.alloc_words(n);
    }
    match process.collect() {
        Ok(collection) => {
            runtime.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
        }
        Err(_) => {
            runtime.halt_process_gc(pid);
            runtime.note_gc_fallback();
            return runtime.alloc_words(n);
        }
    }
    if !scatter_roots_full(process, &mut roots) {
        runtime.halt_process_gc(pid);
        runtime.note_gc_fallback();
        return runtime.alloc_words(n);
    }
    // The collection emptied (or resized) the nursery: retry the bump.
    let fill = runtime.gc_scratch(n.saturating_sub(1));
    if let Ok(block) = bump(process.heap_mut(), n, fill) {
        return Ok(block);
    }
    runtime.note_gc_fallback();
    runtime.alloc_words(n)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the two mechanical pieces of the adoption: the float guard scan and
    //! the safe heap bump. The collection itself is exercised end to end by
    //! `tests/gc_collect.rs` (it needs a real `.beam`, the engine and the scheduler).

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
}
