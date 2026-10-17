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

//! Units for the copy-out's **retry policy** (see
//! [`RuntimeContext::move_result_into_process_heap`]): the growth hint recovered from the
//! copier's own refusal, and the gate that keeps a *permanent* refusal from growing the
//! destination at all. The copy itself is exercised end to end by `tests/perf_call.rs`
//! (path B copies a container result on every iteration) and the `src/context/tests.rs`
//! registered-BIF suites.

use beam_common::{AllocFailure, AllocKind};
use beam_engine::ExecutionContext;
use beam_process::SignalAtoms;
use beam_term::make_small;
use beam_term::prelude::make_header;
use super::*;

/// The number of `Eterm`s a byte figure converts back to.
const WORD: usize = core::mem::size_of::<Eterm>();

/// A `runtime` + a caller-owned `ProcessTable` holding one default process, plus the pid —
/// the `for_process` shape, i.e. the one where the copy-out (and its retry) is ON.
fn process_table(rt: &mut crate::runtime::Runtime) -> (ProcessTable, beam_process::ProcessId) {
    let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
    let mut table = ProcessTable::new(signals);
    let pid = table.spawn_default().expect("a default process");
    (table, pid)
}

#[test]
fn the_growth_hint_is_the_copiers_own_shortfall_in_words() {
    // `beam_message`'s exhaustion error carries `words * size_of::<Eterm>()` in
    // `AllocFailure::requested` (its documented contract), so dividing recovers exactly the
    // flat word count the refused copy wanted — with no second traversal of the graph.
    let exhaustion = BeamError::Alloc(AllocFailure::new(AllocKind::Message, 37 * WORD));
    assert_eq!(
        copy_shortfall_words(&exhaustion),
        37,
        "the copier's own word count is recovered verbatim"
    );
    // A refusal of any other class reports no room request: the conservative `0` (the
    // caller's `max(256)` floor turns it into a legal growth hint).
    assert_eq!(copy_shortfall_words(&BeamError::BadArg), 0);
    assert_eq!(copy_shortfall_words(&BeamError::BadArity), 0);
    assert_eq!(
        copy_shortfall_words(&BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 9 * WORD))),
        9,
        "any `Alloc` refusal carries its own requested byte count"
    );
}

#[test]
fn a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // A bare header word is not a standing term at all (`make_header(0, 0)` is the C
        // ABI's `THE_NON_VALUE`): `copy_struct` answers the copier's typed malformed-graph
        // `badarg` — `BeamError::BadArg`, **not** a message-heap exhaustion. Growing the
        // destination cannot make it succeed, so the retry must not run: no space is
        // retired, no word is written, and the caller gets `None` (so `call_registered`
        // leaves both the result and the pooled BIF heap exactly as they were).
        ctx.move_result_into_process_heap(make_header(0, 0))
    };
    assert!(
        copy.is_none(),
        "a malformed graph cannot be re-homed; the caller must leave the result untouched"
    );
    let process = table.get(pid).expect("the spawned process");
    assert_eq!(
        process.collector().young_space_count(),
        spaces_before,
        "a permanent refusal must not retire a nursery space (the item-4 growth is per-call)"
    );
    assert_eq!(
        process.heap().used(),
        used_before,
        "a refused copy writes no destination word"
    );
}

#[test]
fn an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies() {
    // The positive control for the gate above: a **valid** container the nursery cannot hold
    // still grows the destination by the copier's own word count and is copied on the retry.
    // `lists:seq/2`'s 4000-odd-word result does not fit the default 233-word nursery.
    use beam_engine::ExecutionContext;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let copied = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(2000)])
            .expect("lists:seq(1, 2000)")
    };
    assert!(
        beam_term::prelude::is_list(copied) && !beam_term::literal::is_literal(copied),
        "the result is a real list built on the process's own heap"
    );
    assert!(
        table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count()
            > spaces_before,
        "an exhaustion refusal grows the destination (the retry then fits)"
    );
    // The successful retry's destination was **empty** as handed to the copier (fast path
    // eligible) — `lists:seq/2` carries no off-heap reference, and the retry builds its own
    // fresh table, so nothing from the refused attempt can have poisoned it.
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows),
        (2, 1),
        "one refused attempt, one growth, one retry"
    );
    assert_eq!(
        empties, attempts,
        "every attempt — including the retry — was offered a fully empty destination"
    );
}

/// The copy-out's destination off-heap table is **fresh and empty per attempt**, so
/// `beam_message`'s copier takes its single-traversal fast path even when the runtime's
/// escaped-payload store ([`crate::runtime::Runtime::eval_off_heap`]) is already populated by an
/// unrelated nested run or transport copy.
///
/// **This test fails on the pre-fix code**: with the shared `eval_off_heap` as the copier's
/// destination, the node entry pushed below makes the table non-empty for *both* attempts, so
/// `empty_destination_attempts` reads `0` instead of the `2` asserted here — exactly the
/// permanent poisoning the fresh local table removes.
#[test]
fn the_copy_out_always_hands_the_copier_an_empty_destination_table() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // Model a runtime whose store a prior nested run / transport copy has already populated:
    // one node entry, exactly what a copy that passed an external pid would have registered.
    let node = {
        let atoms = rt.code_server_atoms_mut();
        beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
    };
    rt.eval_off_heap
        .push_node(beam_message::NodeEntry::new(node, 0));
    // A **foreign** container — a term built on a heap the process's collector does not scan —
    // that fits the default nursery by a wide margin (a 2-tuple is 3 words), held in a local so
    // it outlives both copies.
    let mut foreign = beam_alloc::Heap::new(16).expect("a local heap");
    let term = foreign
        .tuple(&[make_small(1), make_small(2)])
        .expect("the tuple fits");
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    notes::reset();
    let (first, second) = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        (
            ctx.move_result_into_process_heap(term),
            ctx.move_result_into_process_heap(term),
        )
    };
    assert!(
        first.is_some() && second.is_some(),
        "both copies fit the nursery"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        empties, 2,
        "each copy was handed a fully empty destination table (the fast path was eligible)"
    );
    assert_eq!(attempts, 2, "one `copy_struct` call per copy-out");
    assert_eq!(grows, 0, "a 3-word tuple never has to grow the nursery");
    let copied_words = table.get(pid).expect("the spawned process").heap().used() - used_before;
    assert_eq!(
        copied_words, 6,
        "two 3-word tuples land on the process's own heap"
    );
}

/// A **refused** copy adopts nothing into the runtime-lifetime store: a malformed-graph refusal
/// leaves [`crate::runtime::Runtime::eval_off_heap`] exactly as it was — its pre-existing entry
/// intact, no entry added — because the destination table is local and is merely dropped.
#[test]
fn a_refused_copy_adopts_nothing_into_the_runtime_store() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // A pre-existing entry models a store a prior nested run populated: the refusal must not
    // disturb it, and must add nothing of its own.
    let node = {
        let atoms = rt.code_server_atoms_mut();
        beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
    };
    rt.eval_off_heap
        .push_node(beam_message::NodeEntry::new(node, 0));
    let nodes_before = rt.eval_off_heap.nodes().len();
    let binaries_before = rt.eval_off_heap.binaries().len();
    notes::reset();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // The `make_header(0, 0)` malformed graph is refused **permanently** (see
        // `a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing`).
        ctx.move_result_into_process_heap(make_header(0, 0))
    };
    assert!(copy.is_none(), "the malformed graph is not re-homed");
    assert_eq!(
        rt.eval_off_heap.nodes().len(),
        nodes_before,
        "a refused copy adopts nothing (it also drops its own node references)"
    );
    assert_eq!(
        rt.eval_off_heap.binaries().len(),
        binaries_before,
        "a refused copy adopts no binary"
    );
    let (attempts, grows, _) = notes::snapshot();
    assert_eq!(attempts, 1, "the malformed graph is refused on the first attempt");
    assert_eq!(grows, 0, "a permanent refusal never grows the nursery");
}

// -----------------------------------------------------------------------
// What the copier registers, and what the copy-out therefore adopts
// -----------------------------------------------------------------------
/// The off-heap payload owner a [`beam_codec::term::make_refc_binary`] call needs — exactly the
/// role `beam_engine`'s `Machine::refc_binaries` plays for a real run.
struct Payloads(Vec<beam_alloc::RefcBinary>);
impl beam_codec::OffHeapBinaries for Payloads {
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm> {
        let handle = beam_alloc::RefcBinary::new(bytes)?;
        let data = handle.data_ptr();
        self.0.push(handle);
        Ok(data.cast::<Eterm>())
    }
}

/// A 200-byte payload — above [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`], so the term built from
/// it is the frozen 8-word `[BinRef][ErlSubBits]` off-heap pair rather than an on-heap binary.
fn payload_bytes() -> Vec<u8> {
    let bytes: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    assert!(bytes.len() > beam_term::refc::ERL_ONHEAP_BINARY_LIMIT);
    bytes
}

/// The off-heap **MSO** reference container `[HEADER_BIN_REF, slot, next]` (the frozen `BinRef`
/// thing, `beam_term::refc`) for slot `slot` on `heap`.
///
/// Written through [`beam_alloc::Heap::used_words_mut`] — the safe, initialised-prefix view — so
/// this crate's `#![deny(unsafe_code)]` holds: no production path builds one (nothing in the
/// workspace produces an MSO yet), and the raw layout owner is `beam_term::refc`, whose
/// on-heap writer is an `unsafe fn`.
fn off_heap_container(heap: &mut beam_alloc::Heap, slot: usize) -> Eterm {
    // A 2-tuple occupies exactly `ERL_BIN_REF_SIZE` words: the header plus its two elements.
    let words = beam_term::prelude::ERL_BIN_REF_SIZE;
    heap.tuple(&vec![beam_term::prelude::NIL; words - 1])
        .expect("the container's words fit");
    let base = {
        let slot_words = heap.used_words_mut();
        assert_eq!(slot_words.len(), words, "the tuple is the whole used prefix");
        slot_words[0] = beam_term::prelude::HEADER_BIN_REF;
        slot_words[1] = Eterm::from_raw(slot);
        slot_words[2] = Eterm::from_raw(0);
        slot_words.as_mut_ptr()
    };
    beam_term::prelude::make_boxed(base)
}

/// **A container holding an off-heap refc binary is copied out byte-faithfully.**
///
/// The copy's source off-heap table is empty (the pooled BIF heap owns no handle), so the copier
/// takes its documented fallback and **flattens** the refc binary's window into a fresh on-heap
/// bitstring: the result is byte-identical, self-contained, and registers **nothing** in the
/// runtime store — the copy-out succeeds where a dangling payload reference would have been the
/// alternative.
#[test]
fn a_container_holding_an_off_heap_refc_binary_is_copied_out_as_an_on_heap_binary() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // The BIF result: a 2-tuple whose first element is an off-heap refc binary — the shape
    // `bs_create_bin`/`base64:encode/1` hands back above the on-heap limit.
    let bytes = payload_bytes();
    let mut source = beam_alloc::Heap::new(64).expect("the source heap");
    let mut payloads = Payloads(Vec::new());
    let binary = beam_codec::term::make_refc_binary(
        &mut source,
        &mut payloads,
        &bytes,
        0,
        bytes.len() * 8,
    )
    .expect("the source refc binary");
    let term = source
        .tuple(&[binary, make_small(1)])
        .expect("the source tuple");
    // What the flattened copy must read back as: the same bytes as an on-heap binary.
    let mut reference_heap = beam_alloc::Heap::new(64).expect("the reference heap");
    let reference_binary =
        beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference binary");
    let reference = reference_heap
        .tuple(&[reference_binary, make_small(1)])
        .expect("the reference tuple");

    let used_before = table
        .get(pid)
        .expect("the spawned process")
        .heap()
        .used();
    notes::reset();
    let copied = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.move_result_into_process_heap(term)
            .expect("the container is re-homed")
    };
    assert!(
        beam_term::prelude::is_boxed(copied) && !beam_term::literal::is_literal(copied),
        "the result is a real tuple built on the process's own heap"
    );
    assert!(
        beam_term::compare::eq(copied, reference, rt.atoms()),
        "the copied binary reads back byte-identical to the payload it was flattened from"
    );
    assert!(
        rt.eval_off_heap.binaries().is_empty(),
        "a flattened payload registers no off-heap handle in the runtime store"
    );
    let copied_words = table.get(pid).expect("the spawned process").heap().used() - used_before;
    assert_eq!(
        copied_words,
        beam_term::size_object(copied),
        "the whole copied graph — the flattened bitstring included — lives on the process heap"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows, empties),
        (1, 0, 1),
        "one attempt, on the empty-destination fast path, with nothing to grow"
    );
}

/// **An off-heap reference with no resolvable owner is refused, never copied into a dangling one.**
///
/// The copy's source table is empty, so an off-heap MSO container's slot does not resolve
/// (`clone_slot` is `None`) and the copier answers its typed `bad_term` — the pool-owning caller
/// then gets `None` (so it neither rewinds the pool nor adopts anything) rather than a term whose
/// `val` word names a slot the runtime store does not hold.
///
/// The refusal has **no root**: the copier's sized fall-back path may already have materialised the
/// container's ancestors (the two words of the tuple below) before it reached the unresolvable
/// slot, and those words are unreachable — nothing in the process's root set points at them — so
/// the invariant asserted here is that no *result* exists, never that the bump pointer is where it
/// was (only the exhaustion preflight, and a refusal of a root that is itself malformed, promise
/// that; see `a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing`).
#[test]
fn an_off_heap_reference_with_no_owner_is_refused_and_adopts_nothing() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let mut source = beam_alloc::Heap::new(64).expect("the source heap");
    let container = off_heap_container(&mut source, 0);
    let term = source
        .tuple(&[container, make_small(1)])
        .expect("the source tuple");
    notes::reset();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.move_result_into_process_heap(term)
    };
    assert!(
        copy.is_none(),
        "the unresolvable container is refused (never re-homed with a dangling slot)"
    );
    assert!(
        rt.eval_off_heap.nodes().is_empty() && rt.eval_off_heap.binaries().is_empty(),
        "a refused copy adopts nothing into the runtime store"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows, empties),
        (1, 0, 1),
        "one attempt, refused on the empty-destination fast path, with nothing to grow"
    );
}

/// A registered BIF whose result is a **container the copier refuses** (`make_header(0, 0)` is the
/// C ABI's non-value, a bare header word), after allocating words on the pooled heap.
fn refused_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let _pooled = ctx.heap().tuple(&[make_small(1), make_small(2)])?;
    Ok(make_header(0, 0))
}

/// A registered BIF that allocates on the pooled heap and returns a **copyable** container.
fn copyable_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let _pooled = ctx.heap().tuple(&[make_small(1), make_small(2)])?;
    ctx.heap().tuple(&[make_small(9)])
}

/// **The pool rewind is a decision of its own, and a refused copy-out leaves the pool alone.**
///
/// `call_registered` rewinds the pooled BIF heap to this call's `mark` **only** when the copy-out
/// reported a container result it could re-home. A refused result is returned to the caller
/// **as it was built** — still on the pool — so rewinding would recycle the very words the result
/// points at. The positive control is the same call shape with a copyable result, whose words
/// *are* given back (to its own `mark`, which is exactly where the refused call left the pool).
#[test]
fn a_refused_copy_out_leaves_the_pooled_bif_heap_unrewound() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    {
        let registry = Rc::get_mut(&mut rt.registry)
            .expect("the runtime holds the only handle to its registry");
        let _ = registry.register("copy_out_fixture", "refused", 0, refused_result);
        let _ = registry.register("copy_out_fixture", "copied", 0, copyable_result);
    }
    let pool_used = |rt: &crate::runtime::Runtime| {
        rt.bif_heaps
            .last()
            .expect("the pool keeps its region")
            .used()
    };
    assert_eq!(pool_used(&rt), 0, "the pool starts clean");
    // The refused result: the BIF's own three words are on the pooled heap when the call returns
    // them, so the pool's bump pointer must still be where the call left it.
    let pool_after_refusal = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "refused", 0, &[])
            .expect("the fixture BIF returns");
        assert_eq!(
            result.raw(),
            make_header(0, 0).raw(),
            "the un-re-homed result is handed to the caller unchanged"
        );
        assert_eq!(
            notes::snapshot(),
            (1, 0, 1),
            "the copy-out did run — one attempt, on the empty-destination fast path — and refused"
        );
        pool_used(ctx.runtime)
    };
    assert_eq!(
        pool_after_refusal, 3,
        "a refused copy-out must not rewind: the result still points into those words"
    );
    // The positive control: the same call shape, a result the copier can re-home — the call's own
    // words are given back, leaving the pool exactly where the previous call left it.
    let pool_after_copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "copied", 0, &[])
            .expect("the fixture BIF returns");
        assert!(
            beam_term::prelude::is_boxed(result) && !beam_term::literal::is_literal(result),
            "the copyable result is a real container on the process heap"
        );
        assert_eq!(
            notes::snapshot(),
            (1, 0, 1),
            "the copy-out ran once on the empty-destination fast path"
        );
        pool_used(ctx.runtime)
    };
    assert_eq!(
        pool_after_copy, pool_after_refusal,
        "a successful copy-out rewinds the pool to its own mark"
    );
}

// ---------------------------------------------------------------------------------------
// The table-less (`-eval`) route's pooled BIF heap — counters first.
//
// The rewind above needs a running process to copy a container result onto. With no process
// (`RuntimeContext::new`, i.e. `-eval`/`-s`/`-run`/the single-pass `run_*` entries) there is no
// destination, so the pool was never rewound: the frontier marched through each region's fixed
// capacity and every fresh page it crossed was touched for the first time. These helpers read
// the pool's own state and the process's minor-page-fault counter, so the cost is *measured*
// (never estimated) and the fix is pinned by a counter, not by a wall time.
// ---------------------------------------------------------------------------------------

/// Every pooled BIF-heap region as `(used, peak_used, capacity)` words, in pool order.
fn pool_snapshot(rt: &crate::runtime::Runtime) -> Vec<(usize, usize, usize)> {
    rt.bif_heaps
        .iter()
        .map(|heap| (heap.used(), heap.peak_used(), heap.capacity()))
        .collect()
}

/// [`pool_snapshot`] in a whitespace-free wire form (`used/peak/capacity` per region, `;`-joined),
/// so the child's single printed line can be split on spaces.
fn compact_regions(rt: &crate::runtime::Runtime) -> String {
    pool_snapshot(rt)
        .iter()
        .map(|(used, peak, capacity)| format!("{used}/{peak}/{capacity}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// The **cumulative high-water** of the pool: the words every region has ever handed out.
///
/// `peak_used` survives a [`beam_alloc::Heap::reset_to`] (it is a property of the live-word
/// history, not of the current offset), so this is exactly "how many pool words have been
/// first-touched", and it is deterministic — no timing, no counter that depends on the host.
fn pool_words_touched(rt: &crate::runtime::Runtime) -> usize {
    rt.bif_heaps.iter().map(|heap| heap.peak_used()).sum()
}

/// This process's minor-page-fault count (`/proc/self/stat` field 10), or `None` where the file
/// does not exist (non-Linux).
fn minflt() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` (field 2) is parenthesized and may itself contain spaces, so the tail is taken
    // after the LAST `)`: it begins at field 3 (`state`), so minflt (field 10) is index 7.
    let tail: Vec<&str> = text.get(text.rfind(')')? + 1..)?.split_whitespace().collect();
    tail.get(7)?.parse().ok()
}

/// One measured row: `N` calls of `body`'s comprehension on the **table-less** route.
struct Row {
    /// Calls (= comprehension elements) in the row.
    calls: usize,
    /// Minor faults the whole child process took (`None` where `/proc/self/stat` is unavailable).
    faults: Option<u64>,
    /// The child's total wall time for the measured expression, in nanoseconds, divided by
    /// `calls` — reported only as a **ratio** between two builds on the same host.
    ns_per_call: f64,
    /// The pool's cumulative high-water ([`pool_words_touched`]) in the child.
    pool_words: usize,
    /// The committed `eval_heap` words in the child.
    store_words: usize,
    /// `pool_snapshot` in the child.
    regions: Vec<(usize, usize, usize)>,
    /// The child's `/proc/loadavg` after the measured expression.
    loadavg: String,
}

impl Row {
    /// This row's minor faults per call (never a wall time).
    fn faults_per_call(&self) -> Option<f64> {
        self.faults.map(|faults| faults as f64 / self.calls as f64)
    }
}

/// The workloads: `(label, body)` — the comprehension body the child evaluates `N` times.
const POOL_WORKLOADS: [(&str, &str); 3] = [
    // The control: an **immediate** result already rewinds the pool on every call.
    ("immediate", "element(1, {I})"),
    // A **container** result: the case the table-less route never rewound.
    ("container", "list_to_tuple([I])"),
    // A container result with a large temporary: the bulk shape, where the pool's marching
    // dominates the call.
    ("bulk", "list_to_tuple(lists:seq(1,20))"),
];

/// The env var that turns this test binary into a **single-row child**.
const POOL_CHILD_ENV: &str = "BEAM_RT_EVAL_POOL_CHILD";

/// `body:calls` → `(body, calls)`.
fn parse_child_spec(spec: &str) -> (String, usize) {
    let (body, calls) = spec.rsplit_once(':').expect("a `<body>:<calls>` spec");
    (body.to_string(), calls.parse().expect("a decimal call count"))
}

/// The **child** half: one fresh process, one measured expression, one printed row.
///
/// A fresh OS process is what makes the fault count mean anything: a pooled region that a
/// previous row already wrote is fully resident, so a same-process measurement would report the
/// *carry-over* rather than the first-touch cost. Every page this child touches is cold.
fn pool_child_row() -> String {
    let spec = std::env::var(POOL_CHILD_ENV).expect("the child spec");
    let (body, calls) = parse_child_spec(&spec);
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    // Warm-up: pre-faults the runtime's own one-time regions (boot, the code server, the
    // evaluator's own first allocations) so the measured window is the workload's own cost.
    rt.eval(&format!("[{body} || I <- lists:seq(1,64)]"))
        .expect("the warm-up expression evaluates");
    let expr = format!("[{body} || I <- lists:seq(1,{calls})]");
    let before = minflt();
    let started = std::time::Instant::now();
    rt.eval(&expr).expect("the measured expression evaluates");
    let elapsed = started.elapsed();
    let after = minflt();
    format!(
        "calls={calls} faults={} ns={} pool_words={} store_words={} regions={} loadavg={}",
        before
            .zip(after)
            .map(|(before, after)| (after - before).to_string())
            .unwrap_or_else(|| "-".to_string()),
        elapsed.as_nanos(),
        pool_words_touched(&rt),
        rt.eval_heap.used(),
        compact_regions(&rt),
        std::fs::read_to_string("/proc/loadavg")
            .map(|load| load.trim().replace(' ', "|"))
            .unwrap_or_else(|_| "unavailable".to_string()),
    )
}

/// Runs the child for `(body, calls)` and parses its row.
fn run_pool_child(body: &str, calls: usize) -> Row {
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            "--ignored",
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "context::calls::tests::eval_route_pool_costs",
        ])
        .env(POOL_CHILD_ENV, format!("{body}:{calls}"))
        .output()
        .expect("the child test process runs");
    assert!(
        output.status.success(),
        "the child row failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The harness prints the test header and the row on the same line (`test … ... calls=…`),
    // so the row is located by its first key rather than by a line start.
    let line = stdout
        .lines()
        .find_map(|line| line.find("calls=").map(|at| &line[at..]))
        .unwrap_or_else(|| panic!("the child printed no row:\n{stdout}"));
    let mut row = Row {
        calls,
        faults: None,
        ns_per_call: 0.0,
        pool_words: 0,
        store_words: 0,
        regions: Vec::new(),
        loadavg: String::new(),
    };
    for field in line.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "faults" => row.faults = value.parse().ok(),
            "ns" => {
                let total: f64 = value.parse().expect("a decimal nanosecond count");
                row.ns_per_call = total / calls as f64;
            }
            "pool_words" => row.pool_words = value.parse().expect("a decimal word count"),
            "store_words" => row.store_words = value.parse().expect("a decimal word count"),
            "regions" => {
                row.regions = value
                    .split(';')
                    .filter(|region| !region.is_empty())
                    .map(|region| {
                        let mut words = region.split('/').map(|word| {
                            word.parse::<usize>().expect("a decimal word count")
                        });
                        (
                            words.next().expect("used"),
                            words.next().expect("peak"),
                            words.next().expect("capacity"),
                        )
                    })
                    .collect();
            }
            "loadavg" => row.loadavg = value.replace('|', " "),
            _ => {}
        }
    }
    row
}

/// Prints the measured rows as a table (measurement harness, not an assertion).
fn report(label: &str, rows: &[Row]) {
    for row in rows {
        println!(
            "[{label}] N={:<7} faults={:<7} faults/call={:<8} ns/call={:<9} pool_words={:<8} store_words={:<8} regions={:?} loadavg={}",
            row.calls,
            match row.faults {
                Some(faults) => faults.to_string(),
                None => "-".to_string(),
            },
            match row.faults_per_call() {
                Some(ratio) => format!("{ratio:.5}"),
                None => "-".to_string(),
            },
            format!("{:.1}", row.ns_per_call),
            row.pool_words,
            row.store_words,
            row.regions,
            row.loadavg,
        );
    }
}

/// **The measurement harness** for the table-less route's pooled BIF heap: minor faults per call,
/// the pool's cumulative high-water and (as a *ratio* only) the per-call wall time, over three
/// `N` so the per-call slope separates from the runtime's constant, for an immediate-result
/// control and two container-result workloads.
///
/// Every row runs in a **fresh child process** (`--ignored --exact` on this very test with
/// [`POOL_CHILD_ENV`] set), because a pooled region written by an earlier row is already
/// resident and would hide the first-touch cost this harness exists to measure.
///
/// ```text
/// taskset -c 3 erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
///     --lib eval_route_pool_costs -- --ignored --nocapture --test-threads=1
/// ```
#[test]
#[ignore = "measurement harness (fresh child process per row, reads /proc/self/stat)"]
fn eval_route_pool_costs() {
    if std::env::var(POOL_CHILD_ENV).is_ok() {
        // Child: print one row and finish (the harness's unit of measurement).
        println!("{}", pool_child_row());
        return;
    }
    const NS: [usize; 3] = [25_000, 50_000, 100_000];
    for (label, body) in POOL_WORKLOADS {
        let rows: Vec<Row> = NS.iter().map(|n| run_pool_child(body, *n)).collect();
        report(label, &rows);
    }
}
