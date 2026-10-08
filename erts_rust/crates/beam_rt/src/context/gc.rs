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

//! The engine's **heap-admission point** (`ExecutionContext::ensure_heap`) and the two
//! allocation seams (`alloc_words`/`heap_available`) for a *scheduled* process.
//!
//! A process running as a scheduler slice allocates on **its own**
//! [`beam_process::Process`] young heap and is collected at the heap-admission point, so its
//! peak memory is bounded by its live set instead of growing with the loop count. Everything
//! outside a scheduled slice keeps the runtime's pointer-stable arena, byte for byte as
//! before — see [`crate::runtime::gc`] for the state, the accounting and the safety valves.
//!
//! # Why the gate is a *per-call* latch
//!
//! The root set this module relocates is the **executing machine's** `x[0..live)` plus its
//! Erlang stack, handed in by [`Machine::ensure_heap`](beam_engine::Machine). A
//! [`RuntimeContext`] is, however, shared by *nested* runs: an external module call
//! ([`RuntimeContext::run_export`]) builds a **new** [`Machine`](beam_engine::Machine) on the
//! same context. A collection driven from that nested machine's admission point would write
//! the *nested* roots into the process's root set and copy the relocated words back into the
//! *nested* machine's slices, leaving the outer machine's terms dangling.
//!
//! The gate is therefore a latch on the context that only the slice's own context wrapper
//! arms ([`BudgetContext::ensure_heap`](crate::runner)) immediately before delegating here,
//! and that this module **takes** ([`std::mem::take`]) as its first statement. Every nested
//! path — a new machine in `run_export`, a fun applied by a bridge, the `-eval` evaluator —
//! is a plain [`RuntimeContext`] whose latch is therefore never set, so it can never collect.
//!
//! # The float guard
//!
//! [`Machine::fr`](beam_engine::Machine) holds **terms** at Milestone 1 and is *not* part of
//! the root set `ensure_heap` hands out, so moving the heap while a float register is live
//! could silently corrupt float code. Two valves keep that impossible:
//!
//! * a module whose code contains a float-family instruction is **never** collected over
//!   ([`module_uses_floats`], cached per module name); everything else — most importantly a
//!   floating-point *BIF* such as `math:sqrt/1`, whose code is in another module — is then
//!   harmless, because that module's own code is never the code under a collection; and
//! * an unresolvable call site (a fun entry or the `-eval` evaluator, which have no
//!   `current_module`) is not collected over either — conservative, never wrong.

use beam_alloc::Heap;
use beam_common::Result;
use beam_gc::CollectionKind;
use beam_loader::{Instr, Module};
use beam_process::{ProcessTable, MAX_REG};
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

/// The engine's heap-admission point for a scheduled slice: collect the running process's
/// own heap when it cannot satisfy `heap_need` more words.
///
/// `armed` is the slice latch (see the module docs). When it is not set, when the running
/// process is not collectable, or when the call site's module is not known to be float-free,
/// this is a no-op — exactly the pre-collector runtime.
///
/// # Errors
///
/// This deliberately has **no** failure mode that could reach the program: a collection is
/// an optimization of an unbounded arena and must never turn a working program into a
/// failing one. A failed (or mis-sized) collection halts collection for that process
/// ([`Runtime::halt_process_gc`](crate::runtime::gc)) and reports success.
pub(crate) fn ensure_heap(
    ctx: &mut RuntimeContext<'_>,
    heap_need: u32,
    live: u32,
    x_regs: &mut [Eterm],
    stack: &mut [Eterm],
    armed: bool,
) -> Result<()> {
    if !armed {
        return Ok(());
    }
    let pid = ctx.mailbox_pid;
    if !ctx.runtime.gc_process_enabled(pid) {
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
    if !process.should_collect(heap_need as usize) {
        return Ok(());
    }

    // Transfer the machine's roots into the process root set. `Process::collect` gathers
    // exactly these containers (registers, then the mailbox, dictionary, saved arguments and
    // pending signals), so writing them here is what makes the machine's live terms survive
    // the move; the relocated words are copied back below.
    let live = (live as usize).min(x_regs.len()).min(MAX_REG);
    let registered = {
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
    };
    if !registered {
        // The root set could not be handed over intact: keep this process's heap put rather
        // than collect against a partial root set.
        runtime.halt_process_gc(pid);
        return Ok(());
    }

    match process.collect() {
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
    let regs = process.registers();
    let relocated_x = regs.x().live_slice();
    let relocated_stack = regs.stack().as_slice();
    if relocated_x.len() != live || relocated_stack.len() != stack.len() {
        runtime.halt_process_gc(pid);
        return Ok(());
    }
    x_regs[..live].copy_from_slice(relocated_x);
    stack.copy_from_slice(relocated_stack);
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
/// tracks the live set. Anything else — an `-eval`/single-pass context, a pinned or halted
/// process, a request the young heap cannot serve, a not-yet-loaded process — falls back to
/// the pointer-stable arena, exactly as before this seam existed. A fallback is never an
/// error: the arena cannot fail where the young heap could.
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
            assert!(is_small(*block.add(8)));
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
