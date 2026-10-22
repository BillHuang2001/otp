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
//! **A nested run's engine block is built on the running process's SCANNED young heap** —
//! the invariant behind the fixed in-process `compile:file/2` abort, pinned profile-independently.
//!
//! # The rule this test pins
//!
//! `src/context/gc.rs`'s `alloc_words_rooted` is the engine's collecting allocation seam. Its
//! **armed** arm belongs to the scheduler slice's own machine; its **unarmed** arm is the
//! *nested* run's door — a cross-module callee's second machine
//! (`RuntimeContext::resolve_external_call`), a fun the engine applied (`apply_fun`), or the
//! `-eval` evaluator, all of which run inside a slice with `RuntimeContext::for_process`'s
//! `gc_armed == false`.
//!
//! The unarmed arm's contract is "behave exactly like the unrooted `alloc_words`": serve the
//! young heap this context names, never collect. It used to `return runtime.alloc_words(n)`
//! — the pointer-stable **arena**, i.e. a region the collector neither scans nor descends
//! into — even though the context still carried the pass's process table. An engine container
//! built there (the compiler's literal-table map key cons, `lib/compiler/src/beam_dict.erl`'s
//! `literal/2`) was therefore invisible to the collector: a later collection relocated the
//! young-heap children it held and the container kept a stale word, which surfaced as garbage
//! at a later map **key** compare — the `class: invalid boxed header subtag` abort (exit 134)
//! that made the in-process compile route address-layout dependent. The arm now delegates to
//! `alloc_words`, so the block comes from the process's own scanned young heap, with the arena
//! a refusal-only fallback.
//!
//! # Why this test, and not only the length sweep
//!
//! `tests/path_length_independence.rs` drives the **real** route at a spread of staging path
//! lengths. That is the end-to-end evidence, but its manifestation is address-layout
//! dependent (which length poisons which container differs per host and per profile), so it
//! cannot state the rule. This test does, through the public API only, with no toolchain, no
//! network and no sleeps:
//!
//! 1. **The build.** A `RuntimeContext` over a live scheduled process (`for_process`, i.e. the
//!    unarmed nested shape) builds a two-element container — a `[alpha | [beta | []]]` cons
//!    chain whose children are heap binaries built through the **unrooted** seam, so they are
//!    young-heap terms on both sides of the fix — with the **rooted** seam
//!    (`ExecutionContext::alloc_words_rooted`), which is exactly the seam that was wrong.
//! 2. **The structure.** The returned block must lie inside a nursery space of the process the
//!    context names (`Process::collector().young_contains`), the process's chain-summed
//!    `heap_used_words()` must have grown by at least the container's own words, and the
//!    runtime must report **no** arena fallback. On the pre-fix code all three fail: the block
//!    lands in the arena and the young heap does not grow at all.
//! 3. **A growth inside the window.** A request larger than the young heap's free words forces
//!    the no-relocation growth (`Process::grow_nursery`) from inside the still-open nested
//!    window; the container must stay where it was and still read back element for element.
//! 4. **The relocation.** The container and both children are published as roots the way a
//!    machine publishes its registers (`Process::registers_mut`'s `x[0..live)`), and a real
//!    `Process::collect` then runs. The container must move with its children: the post-
//!    collection `x[0]` must be a *different* word than the pre-collection one, its elements
//!    must equal the relocated `x[1]`/`x[2]` words **exactly** (a word comparison, so nothing
//!    here dereferences a from-space pointer), and both children's byte payloads must still be
//!    `alpha`/`beta`. A container the collector could not descend into would keep the stale
//!    pre-collection child words and fail that comparison.
//!
//! This file is its own crate root (an integration-test binary): `beam_rt` is
//! `#![deny(unsafe_code)]`, and this suite keeps that discipline everywhere except the **one**
//! block that lays the cons cells out in the words the seam handed back — the same shape
//! `beam_engine`'s own `write_list_at` uses, carrying its own `// SAFETY:` rationale.
use beam_engine::terms;
use beam_engine::context::RootSet;
use beam_engine::ExecutionContext;
use beam_process::{ProcessTable, SignalAtoms};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{make_list, AtomTable, Eterm, NIL};
/// The first child's byte payload: a small binary, so it is a **young-heap** `heap_bits` block
/// (a `binary:copy/1`-sized payload of the same shape in the real route's container key).
const ALPHA: &[u8] = b"alpha";
/// The second child's byte payload.
const BETA: &[u8] = b"beta";
/// The words the container's single reservation asks for: two cons cells (`[CAR, CDR]` each),
/// allocated as one contiguous block — the layout `terms::write_list_at` produces and the
/// collector walks through the `CDR` chain.
const CONTAINER_WORDS: usize = 4;
/// What a nested run's container is called in this file's messages.
const CONTAINER: &str = "the container built inside the nested run";
/// **The invariant: a live heap serves its own builds — a nested run's container must land on the
/// running process's scanned young heap, survive a growth, and move with its children in a real
/// collection.**
///
/// See the module docs for what each phase asserts and why each one fails on the pre-fix code
/// (`return runtime.alloc_words(n)` in `alloc_words_rooted`'s unarmed arm).
#[test]
fn a_nested_runs_container_is_built_on_the_processes_scanned_young_heap() {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let mut atoms = AtomTable::new();
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
    let pid = table
        .spawn_default()
        .expect("a process in the pass table");
    // -----------------------------------------------------------------------------------
    // 1. The children: the UNROOTED seam, which serves the young heap on both sides of the fix.
    // -----------------------------------------------------------------------------------
    let (alpha, beta) = {
        let mut ctx = RuntimeContext::for_process(&mut runtime, &mut table, pid);
        let alpha = terms::make_heap_binary(&mut ctx, ALPHA).expect("the first child is built");
        let beta = terms::make_heap_binary(&mut ctx, BETA).expect("the second child is built");
        (alpha, beta)
    };
    let used_before_container = table
        .get(pid)
        .expect("the process is still in the table")
        .heap_used_words();
    let fallbacks_before = runtime.gc_stats().fallbacks;
    // -----------------------------------------------------------------------------------
    // 2. The container: the ROOTED seam inside a nested window (`for_process` is unarmed).
    // -----------------------------------------------------------------------------------
    let mut x = [NIL; 4];
    let mut stack = [NIL; 2];
    let mut fr = [NIL; 4];
    let container = {
        let mut ctx = RuntimeContext::for_process(&mut runtime, &mut table, pid);
        let roots = RootSet::new(&mut x[..], &mut stack[..], &mut fr[..], 1);
        let hp = ctx
            .alloc_words_rooted(CONTAINER_WORDS, roots)
            .expect("the container's block is served");
        // SAFETY: `alloc_words_rooted` returned `CONTAINER_WORDS` exclusively-owned, writable
        // words (uninitialized storage valid until the next admission point or collecting
        // allocation, and nothing may allocate between this write and the `make_list` below).
        // The two cells are written as the header-less `[CAR, CDR]` pair the frozen term layout
        // requires — the first CDR points at the second cell, the second CDR is `NIL` — so the
        // whole block is exactly one proper two-element list and no word is left uninitialized.
        unsafe {
            *hp = alpha;
            *hp.add(1) = make_list(hp.add(2));
            *hp.add(2) = beta;
            *hp.add(3) = NIL;
        }
        make_list(hp)
    };
    let process = table.get(pid).expect("the process is still in the table");
    // The structural half: the block is inside a nursery space of the process the context names.
    // PRE-FIX: the block is arena memory, so this assertion is the one that fails first.
    assert!(
        process.collector().young_contains(container.raw() as *const Eterm),
        "{CONTAINER} must be served from a nursery space of the process the nested context \
         names, not from the pointer-stable arena (term {:#x})",
        container.raw()
    );
    // ... and the process's own heap accounting sees it.
    let used_after_container = process.heap_used_words();
    assert!(
        used_after_container >= used_before_container + CONTAINER_WORDS,
        "the nested run's {CONTAINER_WORDS}-word block must grow the process's used heap words \
         (`heap_used_words`: {used_before_container} before the container, {used_after_container} \
         after), because a block served from the arena is invisible to the collector"
    );
    // ... and nothing fell back to the arena while the window was open.
    assert_eq!(
        runtime.gc_stats().fallbacks,
        fallbacks_before,
        "a nested run's rooted allocation must not fall back to the pointer-stable arena"
    );
    // -----------------------------------------------------------------------------------
    // 3. A growth INSIDE the still-open nested window: the container keeps its home and value.
    // -----------------------------------------------------------------------------------
    {
        let mut ctx = RuntimeContext::for_process(&mut runtime, &mut table, pid);
        // One word more than the young heap can serve, so the seam takes its capacity-refusal
        // path: the no-relocation growth (`Process::grow_nursery`) and a retried bump.
        let need = ctx.heap_available() + 1;
        ctx.alloc_words(need)
            .expect("the growth inside the nested window serves its block");
    }
    let process = table.get(pid).expect("the process is still in the table");
    assert!(
        process.collector().young_contains(container.raw() as *const Eterm),
        "a growth inside the nested window retires the space it replaces; a word it handed out \
         must stay addressable at its original address"
    );
    assert_eq!(
        terms::list_elements(container).expect("the container is a proper list"),
        vec![alpha, beta],
        "a no-relocation growth moves nothing, so the container still reads back its elements"
    );
    // -----------------------------------------------------------------------------------
    // 4. A real collection: the container must move WITH its children.
    // -----------------------------------------------------------------------------------
    {
        let process = table.get_mut(pid).expect("the process is still in the table");
        let regs = process.registers_mut();
        // The live prefix first: `XRegs::set` writes only inside `x[0..live)` (the C's
        // `regs[0..live]` root prefix), exactly as a machine publishes its registers.
        regs.x_mut()
            .set_live(3)
            .expect("three live x registers");
        regs.x_mut().set(0, container).expect("x0 holds the container");
        regs.x_mut().set(1, alpha).expect("x1 holds the first child");
        regs.x_mut().set(2, beta).expect("x2 holds the second child");
    }
    let collection = table
        .get_mut(pid)
        .expect("the process is still in the table")
        .collect()
        .expect("the collection over the published root set runs");
    assert!(
        collection.stats.words_copied > 0,
        "the collection evacuated live words (so the relocation below is not vacuous): {:?}",
        collection.stats
    );
    let process = table.get(pid).expect("the process is still in the table");
    let x_after = process.registers().x();
    let moved_container = x_after.get(0).expect("x0 is readable");
    let moved_alpha = x_after.get(1).expect("x1 is readable");
    let moved_beta = x_after.get(2).expect("x2 is readable");
    assert_ne!(
        moved_container.raw(),
        container.raw(),
        "the collection relocated the container itself — it was a scanned young-heap term"
    );
    // The container's interior was relocated in lockstep with the roots, so every element is
    // **word-identical** to the relocated root of the same child. A container the collector
    // could not descend into (the pre-fix arena block) would still hold the stale pre-collection
    // child words here.
    assert_eq!(
        terms::list_elements(moved_container).expect("the relocated container is a proper list"),
        vec![moved_alpha, moved_beta],
        "the container's own children must be relocated with it"
    );
    assert_eq!(
        terms::binary_bytes(moved_alpha).expect("the first child is a byte-aligned binary"),
        ALPHA,
        "the first child's payload survived the collection"
    );
    assert_eq!(
        terms::binary_bytes(moved_beta).expect("the second child is a byte-aligned binary"),
        BETA,
        "the second child's payload survived the collection"
    );
    assert_ne!(
        moved_alpha.raw(),
        alpha.raw(),
        "the children really did move, so the element comparison above is a relocation test"
    );
}
