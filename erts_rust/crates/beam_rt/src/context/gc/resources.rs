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

//! The **NIF-resource prune** — the reachability decision contract A.3 needs so a resource term
//! that only an Erlang term kept alive gets its destructor run.
//!
//! # Ground truth
//!
//! `erts/emulator/beam/erl_nif.c`: `enif_make_resource` (`:3197`) mints an `ERTS_MAGIC_REF` whose
//! block owns one reference on the resource; the term GC drops that reference when the term
//! becomes unreachable, and the release that takes the count to zero runs the type's `dtor`
//! (`nif_resource_dtor_prologue` → `run_resource_dtor`, `:2961-3030`). `beam_abi` mints the term
//! and takes the reference (`beam_abi::nif::enif::resources`), but it has no GC — *which* terms are
//! still reachable is only knowable by the host, so the seam is a caller-supplied predicate
//! (`beam_abi::nif::enif::resources::release_resource_terms`). **This module is that caller.**
//!
//! Without it the destructor never runs for a resource exposed through a term: the reference the
//! term took is never dropped. That was the measured divergence (oracle destructor count `1`,
//! `beam_rt` `0`), and it is a **bounded leak**, never a wrong result — which is also exactly the
//! direction every gate below declines in.
//!
//! # The site, and why it is the only sound one
//!
//! [`reclaim`] is called from the runner's `Yield` park arm, beside the off-heap refc-binary
//! prune and under the *same* `engine.parked_published` gate
//! ([`RuntimeRunner::reclaim_off_heap_binaries`](crate::runner)). A prune may only run against the
//! process's **complete** root set, and the park is the one point that has it: `park_machine`
//! published the machine's whole root set (`RootSet::ALL_LIVE` — every live x register, the whole
//! Erlang stack, the FR file at its pointer-bearing prefix) into the process's registers
//! immediately before this call. An **exempt** park (the `Wait`/receive arm) publishes nothing, so
//! `parked_published` is `false` there and the prune does not run — its terms stay leaked rather
//! than being freed behind a live machine's back.
//!
//! The prune must **never** be called from:
//!
//! * a **heap-admission door** inside a slice — the executing machine's root set is not published
//!   there, and the door has no `&mut Machine` to publish it from;
//! * a **BIF** — a BIF's arguments live on a pooled heap outside the process's root set;
//! * the **`-eval` route** — the action process is never enqueued, so it never parks. That route
//!   is **not** left unreclaimed: it has its own two sites, in [`crate::eval::reclaim`]'s resource
//!   half — the `-eval` statement boundary
//!   ([`crate::eval::reclaim::prune_at_statement_boundary`], between the evaluator's top-level
//!   statements) and a **nested run**'s epilogue
//!   ([`crate::eval::reclaim::prune_beyond`], from `RuntimeContext::adopt_nested_payloads`, which a
//!   cross-module call and a fun application both reach). Both run the **same** reachability
//!   descent and the same two-tier (word, then reference-identity) match this module's walk uses,
//!   under a different soundness device — a **frontier**, so a site releases only candidates minted
//!   inside its own window, because neither site can enumerate its whole process the way a park
//!   can. This module stays the scheduled-process site and its gates below stay exactly as they
//!   are.
//!
//! # Cadence
//!
//! The prune is **park-driven**, not collection-driven: a process parks at the end of a slice that
//! exhausted its reduction budget (`DEFAULT_REDUCTIONS_PER_SLICE = CONTEXT_REDS = 4000`) or that
//! blocked. So a resource term is reclaimed at the first non-exempt park *after* the last root
//! naming it died — which a target reaching a `receive` or running longer than one slice does
//! naturally, and which a straight-line function that returns immediately never does (it has no
//! park). That is a **timing** difference from the oracle, not a correctness one: the destructor
//! runs at the next park rather than inside `erlang:garbage_collect/0`.
//!
//! # The gates (every one is a decline, and a decline is a leak)
//!
//! 1. **No live resource terms** — the overwhelmingly common case (no NIF ever minted one). One
//!    registry read, and **no heap walk at all**.
//! 2. the process is in the caller's table;
//! 3. [`Process::is_relocation_exempt`] is `false`;
//! 4. [`Runtime::gc_process_enabled`] — the pinned/halted/escape-valve gate every collection
//!    point honours;
//! 5. the **prune-only valve** ([`super::reaches_an_unscanned_word`]) — *any* pointer-bearing word
//!    reachable from the roots in a region the process's collector does not scan. This is the
//!    soundness gate for this walk: a resource term inside a container from such a region is not
//!    reliably reachable through the collector's geometry, so the prune declines **everything**
//!    rather than answering "unreachable" for a resource a live foreign container still holds. It
//!    is classified over the **process's own** roots alone; the host root sources below are seeded
//!    into the reachability walk but deliberately kept out of this valve, because every host store
//!    is a foreign region by construction and the valve would therefore decline every prune a
//!    resource program ever attempts.
//!
//! # The residual (leak, never a dangle)
//!
//! The walk's roots are the process's own root set **plus** every host root source
//! [`Runtime::gather_off_heap_roots`] enumerates that is live **during a scheduler pass** — the
//! escaped-closure environments, the `seq_trace` tokens, the pending-spawn records and every pending
//! timer **delivery** — so a resource named only by one of those stores is reached and retained.
//! The two families that live in a per-pid table are **empty at this arm**, and that is a property
//! of the pass rather than of the enumeration: a scheduler pass moves the process table *and*
//! [`crate::runner::RunnerState`] out of the [`Runtime`] for its duration
//! (`Runtime::take_processes`), so the enumeration here sees an equivalent **empty** table and a
//! default runner state. Both families are live at the two **non-park** sites
//! ([`crate::eval::reclaim::resource`]'s statement boundary and nested-run epilogue), which run with
//! the runtime's own tables. Consequences:
//!
//! * **another live process's** registers, saved arguments, signals, mailbox and dictionary are not
//!   visible at this arm, so a resource held *only* by another process's register is answered
//!   unreachable here — the same class as one held only by another process's heap. The executing
//!   process's own registers are this arm's **local** root list (below), not a host seed;
//! * an **engine's** un-consumed spawn arguments and an un-published parked machine's root set
//!   ([`crate::runner::RunnerState::gather_off_heap_roots`]'s two sub-families) are likewise empty
//!   here, for the same reason: a child spawned through the pass's pending-spawn route keeps its
//!   arena-stable arguments rooted at the two **non-park** sites, never here. An **exempt** park
//!   (every `receive`) never reaches this arm at all — gate 3 below declines it — so a machine
//!   parked in one keeps its resource terms instead of having them freed behind its back;
//! * **a copy is reached through its reference identity**: a resource term is minted as an ordinary
//!   internal reference whose `id` comes from the one shared local reference-id space, so a
//!   `copy_struct` copy of it (a different word, a different region) is still reached — which is
//!   the correct direction — and no *unrelated* plain reference can carry that id;
//! * a `persistent_term` value and an **ETS object** are not enumerated at all: their copies keep a
//!   resource's reference identity, so a resource held *only* by one of them is answered
//!   unreachable and released. Closing ETS needs an object iterator the `beam_bifs` table registry
//!   does not expose;
//! * a resource held only by a **foreign** container (a pooled-BIF tuple, an arena fallback) is
//!   kept alive by gate 5, forever if nothing ever drops that container;
//! * a candidate whose **reference identity cannot be read** (its minted block no longer holds a
//!   reference header) is recorded as reached rather than released: "cannot identify it" must never
//!   become "it is dead", so that resource stays leaked for the runtime's life;
//! * a resource term that was **copied** (the send path's `copy_struct`, an ETS insert) keeps its
//!   object alive through the copy's identity — [`super::super::gc_walk::reachable_resource_words`]
//!   matches a reached reference term's id against the candidates' — so a copy that leaves no
//!   reachable word and no identity behind is the only shape a release can still miss, and a
//!   recycled-in-place minted block can only *narrow* the match (a leak), never invent one.
use beam_process::{Process, ProcessId, ProcessTable};
use beam_term::Eterm;

use beam_abi::nif::enif::resources::{release_resource_terms, resource_terms};

use crate::runtime::Runtime;
use beam_common::Result;

/// Drops the reference held by every live NIF **resource term** that nothing reachable from `pid`'s
/// complete root set names, returning how many terms were released.
///
/// The caller is the runner's `Yield` park arm, for a **publishing** window (see the module docs
/// for the site and the cadence, and for the list of sites where this must never be called).
///
/// A released term's binding is removed (so a later `enif_get_resource` on it answers `0`) and
/// exactly one reference is dropped from its object, through the same path `enif_release_resource`
/// takes — so the resource type's destructor runs, synchronously, when that was the last
/// reference. An object that still carries an allocator or `enif_keep_resource` reference survives
/// with its count reduced by one and no destructor run.
///
/// # Errors
///
/// None are produced: every failure mode is the documented decline. The `Result` matches the
/// sibling collection doors' shape so the runner's arm reads uniformly.
pub(crate) fn reclaim(runtime: &mut Runtime, table: &mut ProcessTable, pid: ProcessId) -> Result<usize> {
    // Gate 1: the common case, and the reason this costs nothing for a program that never minted a
    // resource — one registry read, no walk.
    let candidates: Vec<usize> = resource_terms().iter().map(|term| term.raw()).collect();
    if candidates.is_empty() {
        return Ok(0);
    }
    // The host's root sources that are live **during** a scheduler pass — the escaped-closure
    // environments, the `seq_trace` tokens, the pending-spawn records and every pending timer
    // **delivery**. Gathered **before** the process borrow below, because `table` and `runtime` are
    // separate parameters and only this order keeps both unborrowed at once.
    //
    // The two per-pid families are empty here and cannot be otherwise: the pass moved the process
    // table (`Runtime::take_processes`, so the runtime's own is an equivalent empty placeholder) and
    // `RunnerState` out of the runtime, which is exactly when this arm runs. They are live at the
    // two **non-park** sites (`crate::eval::reclaim::resource`), which run with the runtime's own
    // tables. The executing process's own root set is this arm's local list below.
    //
    // These words are handed to the **reachability walk only** (below), never to gate 5: a host
    // store lives outside this process's scanned heap by definition, so every one of its container
    // words is a `WordRegion::Foreign` root, and the payload valve declines on *any* such word —
    // folding them in would decline every prune a resource program ever attempts.
    let mut host: Vec<Eterm> = Vec::new();
    runtime.gather_off_heap_roots(&mut host);

    let Some(process) = table.get_mut(pid) else {
        return Ok(0);
    };
    if process.is_relocation_exempt() {
        return Ok(0);
    }
    if !runtime.gc_process_enabled(pid) {
        return Ok(0);
    }

    // The process's **complete** root set, gathered exactly as the off-heap prune gathers it and
    // exactly as `Process::collect` does: the registers (`x[0..live)`, the whole stack and the
    // host-published extra roots), each queued message's `payload`/`from`/`token`, the dictionary,
    // the saved arguments and the pending signals.
    let mut roots: Vec<Eterm> = Vec::new();
    {
        let process: &Process = process;
        process.registers().gather_roots(&mut roots);
        for message in process.mailbox().iter() {
            roots.push(message.payload());
            roots.push(message.from());
            roots.push(message.token());
        }
        process.dict().gather_roots(&mut roots);
        process.saved_args().gather_roots(&mut roots);
        process.signals().gather_roots(&mut roots);
    }

    // Gate 5: the prune-only valve. A word reachable from the roots in a region the collector does
    // not scan may sit in a container whose children this walk must not trust, so *any* of them
    // declines the whole prune: the residual is a leak, never a dangling payload.
    if super::reaches_an_unscanned_word(runtime.walk_scratch(), process, roots.iter()) {
        runtime.note_foreign_reachable(pid);
        crate::debug_trace::trace(format_args!(
            "resource prune declined for {pid:?}: a root reaches a word outside the scanned region"
        ));
        return Ok(0);
    }

    // The reachability decision itself: the reachable subset of `candidates`, over both regions, through
    // cons cells (so a resource held in `[Res]` or in a message payload is found), and by **reference
    // identity** as well as by word — a `copy_struct` copy (the send path, ETS) is a different word with
    // the same id, and it designates the same object. The host root sources are seeded into the same
    // walk, so a resource named **only** by a pending timer delivery (or a pending spawn, a
    // `seq_trace` token, an escaped closure's environment) is reached and kept.
    let reached = super::super::gc_walk::reachable_resource_words(
        runtime.walk_scratch(),
        process,
        roots.iter(),
        host.iter(),
        &candidates,
    );

    // A candidate that was not reached is dead: drop the reference its term took. `reached` is sorted
    // (the walk's entry point guarantees it) and holds a candidate's own raw word whenever anything
    // reachable names it — directly, through a copy's identity, or because the walk could not read the
    // candidate's identity and therefore declined to call it dead.
    let released = release_resource_terms(|term| reached.binary_search(&term.raw()).is_ok());
    if released > 0 {
        crate::debug_trace::trace(format_args!(
            "resource prune for {pid:?}: released {released} of {} live resource term(s)",
            candidates.len()
        ));
    }
    Ok(released)
}
