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

//! The **root-coverage parity suite**: the mutable root enumeration against the value gather.
//!
//! # Why this file exists
//!
//! The runtime publishes one root set through **two independent implementations**:
//!
//! * [`Runtime::gather_off_heap_roots`] — the *value* gather, a read-only corpus of every term
//!   the runtime keeps alive (what a conservative prune seeds its walk from);
//! * [`Runtime::for_each_root_family_mut`] / [`Runtime::for_each_root_mut`] — the *mutable* walk,
//!   which hands the words themselves over so a future relocation/compaction pass can rewrite a
//!   root in place instead of only learning that it exists.
//!
//! Two implementations of one set drift. This suite pins that they do not: for a set of runtime
//! states it asserts the value gather's corpus is exactly the multiset union of the families
//! [`RootFamily::in_value_gather`] marks `true`, and that the remaining families are a real,
//! populated superset (a root the mover must rewrite but the conservative gather deliberately
//! skips).
//!
//! **A failure here means a root is missing from the mutable walk** — not a cosmetic mismatch.
//! A word the gather knows about but the walk never visits is a word a mover would leave pointing
//! at the address it evacuated: a use-after-free the moment a compacting pass lands. The
//! comparison is therefore **order-insensitive and multiplicity-aware** (a sorted multiset of raw
//! word values), because the two walks deliberately visit the families — and a mailbox message's
//! envelope words — in different orders.
//!
//! # States
//!
//! Nine states, one builder each, together covering every family the walk can reach from a
//! unit test. Three of them populate runtime state that has **no public constructor** by writing
//! the runtime's own private fields directly (this module is a child of `crate::runtime`, so
//! `rt.runner_state`, `rt.pending_spawns`, `rt.trace` and `rt.persistent_terms` are in scope):
//! that is deliberate — the point of the state is the *shape of the root set*, not the route that
//! produced it, and no production seam exists to reach those holders from a test.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use beam_message::Message;
use beam_process::ProcessId;
use beam_term::{make_small, Eterm, NIL};

use super::roots::RootFamily;
use super::{PendingSpawn, Runtime};
use crate::runner::ProcessEntry;
use crate::timers::TimerTarget;
use crate::trace::{pattern_flags_from_atoms, MatchSpec, MfaPattern};

// ---------------------------------------------------------------------------
// The five helpers
// ---------------------------------------------------------------------------

/// The comparison key: the words' **raw values**, sorted.
///
/// `Eterm`'s identity is its raw word (its `Hash`/`Eq` are the word), so a multiset of raw values
/// is exactly the "same roots, whatever order" property the two walks must share.
fn multiset(words: Vec<Eterm>) -> Vec<usize> {
    let mut raw: Vec<usize> = words.into_iter().map(Eterm::raw).collect();
    raw.sort_unstable();
    raw
}

/// The value gather's corpus, as a multiset.
fn value_gather(rt: &mut Runtime) -> Vec<usize> {
    let mut out: Vec<Eterm> = Vec::new();
    rt.gather_off_heap_roots(&mut out);
    multiset(out)
}

/// The mutable walk's corpus restricted to the families the value gather visits — the multiset the
/// core parity assertion compares against [`value_gather`].
fn shared_walk(rt: &mut Runtime) -> Vec<usize> {
    let mut out: Vec<Eterm> = Vec::new();
    rt.for_each_root_family_mut(&mut |family, words| {
        if family.in_value_gather() {
            out.extend_from_slice(words);
        }
        Ok(())
    })
    .expect("the root walk visits every family");
    multiset(out)
}

/// Every word the mutable walk visits, with the family that owns it.
fn all_walk(rt: &mut Runtime) -> Vec<(RootFamily, usize)> {
    let mut out: Vec<(RootFamily, usize)> = Vec::new();
    rt.for_each_root_family_mut(&mut |family, words| {
        for word in words {
            out.push((family, Eterm::raw(*word)));
        }
        Ok(())
    })
    .expect("the root walk visits every family");
    out
}

/// How many visitor calls each family received — the *coverage* count (a family the walk never
/// reaches is invisible to a multiset comparison when it is also absent from the value gather).
fn per_family_counts(rt: &mut Runtime) -> BTreeMap<RootFamily, usize> {
    let mut counts: BTreeMap<RootFamily, usize> = BTreeMap::new();
    rt.for_each_root_family_mut(&mut |family, _words| {
        *counts.entry(family).or_insert(0) += 1;
        Ok(())
    })
    .expect("the root walk visits every family");
    counts
}

// ---------------------------------------------------------------------------
// State builders
// ---------------------------------------------------------------------------

/// A plain runtime: the three never-enqueued runtime-owned processes and nothing else.
fn bare_state() -> (&'static str, Runtime) {
    ("bare", Runtime::with_defaults().expect("the default runtime"))
}

/// A booted runtime: the 22 preloaded modules are loaded, so any engine/`system_info` state the
/// boot sequence leaves behind is in scope.
fn booted_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the preload boot");
    ("booted", rt)
}

/// A runtime holding one live process whose root containers are all populated.
///
/// The mailbox's `from`/`token` words must be immediates (`Queue::enqueue`'s debug assertion), so
/// the envelope is a real pid term; the payloads are distinct boxed terms stabilized on the
/// runtime's own store.
fn live_process_state() -> (Runtime, ProcessId) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let pid = rt
        .processes
        .spawn_default()
        .expect("a live process slot");
    let dict_key = rt.intern("root_coverage_key");
    let dict_value = stable_term(&mut rt, "{dict, value}");
    let payload_a = stable_term(&mut rt, "{message, a}");
    let payload_b = stable_term(&mut rt, "{message, b}");
    let register_word = stable_term(&mut rt, "{register, 1}");
    let saved = stable_term(&mut rt, "{saved, arg}");
    let template = stable_term(&mut rt, "{timer, message}");
    let from = rt.intern("timer_sender");
    let timer_ref = rt.intern("timer_reference");
    let process = rt
        .processes
        .get_mut(pid)
        .expect("the process just spawned");
    process.dict_mut().put(dict_key, dict_value);
    process.mailbox_mut().enqueue(Message::new(payload_a, NIL));
    process.mailbox_mut().enqueue(Message::new(payload_b, NIL));
    process.saved_args_mut().push(saved);
    let registers = process.registers_mut();
    registers
        .x_mut()
        .set_live(2)
        .expect("two live x registers");
    registers.x_mut().set(0, register_word).expect("x(0)");
    registers.x_mut().set(1, dict_key).expect("x(1)");
    registers.extra_roots_mut().push(dict_value);
    // A far-future deadline: the delivery family is populated and nothing ever fires.
    rt.timers_mut().schedule(
        Duration::from_secs(1 << 20),
        TimerTarget::Pid(pid),
        from,
        template,
        timer_ref,
    );
    (rt, pid)
}

/// A runtime with one escaped `-eval` closure, so `EvalClosureEnv` is non-empty.
///
/// The capture environment is the evaluator's whole `Env`, so an assignment before the `fun`
/// literal is what puts a term in it.
fn eval_closure_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.eval("Y = {captured, value}, fun(X) -> {X, Y} end")
        .expect("a closure over one captured term");
    assert!(
        !rt.eval_closures.is_empty(),
        "the -eval route registered a closure"
    );
    ("eval-closure", rt)
}

/// A runtime whose `RunnerState` holds a recorded **result** (and, opportunistically, a recorded
/// **exit**), produced by really running processes through the scheduler.
fn runner_recorded_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(slice_fixture()).expect("slice.beam loads");
    let pid = rt
        .spawn_process("slice", "fib", vec![make_small(10)])
        .expect("spawn slice:fib/1");
    // A second process that raises, so `RunnerExit` is populated too; its outcome is not asserted
    // (the family is covered by the parity assertion either way).
    let _ = rt.spawn_process("slice", "pick", vec![make_small(99)]);
    let _ = rt.run_scheduler().expect("the pass runs");
    assert_eq!(
        rt.process_result(pid).map(beam_term::signed_value),
        Some(55),
        "the recorded result family is real"
    );
    ("runner-recorded", rt)
}

/// A runtime with one recorded-but-not-yet-absorbed spawn: an `Export` entry with a non-empty
/// argument vector.
fn pending_spawn_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(slice_fixture()).expect("slice.beam loads");
    let module: Arc<beam_loader::Module> = rt
        .code_server()
        .modules()
        .find(|module| module.atom_name(module.name) == Some("slice"))
        .cloned()
        .expect("the loaded slice module");
    let function = module
        .atom_table
        .lookup("fib")
        .expect("fib is in the module's atom table");
    let arg = stable_term(&mut rt, "{spawn, arg}");
    let pid = rt.processes.spawn_default().expect("a child pid slot");
    rt.pending_spawns.push(PendingSpawn {
        pid,
        entry: ProcessEntry::Export {
            module,
            function,
            arity: 1,
        },
        args: vec![arg],
    });
    ("pending-spawn", rt)
}

/// A runtime with a `persistent_term` entry holding a **boxed** value — the one family whose
/// presence in the mutable walk and absence from the value gather is the superset's clearest case.
fn persistent_term_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let key = rt.intern("root_coverage_persistent");
    let value = stable_term(&mut rt, "{persistent, value}");
    let store = rt.persistent_terms.clone();
    assert!(
        store
            .borrow_mut()
            .put(key, value, false, rt.atoms()),
        "the entry was stored"
    );
    ("persistent-term", rt)
}

/// A runtime with an installed function trace pattern carrying a `MatchSpec::Functions` body.
fn trace_match_spec_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let first = rt.intern("match_first");
    let second = rt.intern("match_second");
    let flags = pattern_flags_from_atoms(&[]).expect("the empty flag list");
    rt.trace_mut().patterns_mut().install(
        MfaPattern {
            module: Some("slice".to_string()),
            function: Some("fib".to_string()),
            arity: Some(1),
        },
        MatchSpec::Functions(vec![first, second]),
        flags,
    );
    ("trace-match-spec", rt)
}

/// A runtime with one live built-in driver port carrying `port_set_data/2` data.
fn port_data_state() -> (&'static str, Runtime) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let (id, _flags) = crate::port::test_support::register_control_port(
        &mut rt,
        "root_coverage_driver",
        Vec::new(),
    );
    let data = stable_term(&mut rt, "{port, data}");
    rt.ports
        .borrow_mut()
        .set_port_data(id, data)
        .expect("the port is live");
    ("port-data", rt)
}

/// Every state the suite exercises, each built freshly.
fn states() -> Vec<(&'static str, Runtime)> {
    let (live, _pid) = live_process_state();
    vec![
        bare_state(),
        booted_state(),
        ("live-process", live),
        eval_closure_state(),
        runner_recorded_state(),
        pending_spawn_state(),
        persistent_term_state(),
        trace_match_spec_state(),
        port_data_state(),
    ]
}

// ---------------------------------------------------------------------------
// Fixtures and term helpers
// ---------------------------------------------------------------------------

/// The committed loader fixture, reused by relative path (never copied).
fn slice_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    )
}

/// A **runtime-lifetime** copy of the term `expr` evaluates to.
///
/// [`Runtime::stabilize_term`] copies onto the never-reset store, which is what makes the word
/// safe to hand a root container (a raw `-eval` result lives on the statement window).
fn stable_term(rt: &mut Runtime, expr: &str) -> Eterm {
    let term = rt.eval(expr).expect("the fixture expression evaluates");
    rt.stabilize_term(term).expect("the term stabilizes")
}

/// The multiset `sub` is contained in `sup`, and `sup` holds at least one word `sub` does not.
fn assert_strict_superset(sub: &[usize], sup: &[usize], state: &str) {
    let mut remaining = sup.to_vec();
    for word in sub {
        let Some(index) = remaining.iter().position(|candidate| candidate == word) else {
            panic!(
                "state {state}: the value gather's word {word:#x} is missing from the mutable walk \
                 — a root the mover would leave dangling (gather {} words, walk {} words)",
                sub.len(),
                sup.len()
            );
        };
        remaining.remove(index);
    }
    assert!(
        !remaining.is_empty(),
        "state {state}: the mutable walk must hold at least one extra word beyond the value \
         gather (both hold {} words)",
        sub.len()
    );
}

// ---------------------------------------------------------------------------
// 1. The core guarantee
// ---------------------------------------------------------------------------

/// **The** parity assertion: in every state, the value gather's corpus is exactly the multiset
/// union of the families [`RootFamily::in_value_gather`] marks `true`.
#[test]
fn the_family_walk_matches_the_value_gather_in_every_state() {
    for (name, mut rt) in states() {
        let gathered = value_gather(&mut rt);
        let walked = shared_walk(&mut rt);
        assert_eq!(
            walked, gathered,
            "state {name}: the mutable walk's shared families must cover the value gather exactly \
             (gather {} words, walk {} words)",
            gathered.len(),
            walked.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The documented superset is real
// ---------------------------------------------------------------------------

/// The families the value gather deliberately skips are **populated**, not aspirational: each
/// asserted state carries the expected word under the expected family, and the whole walk is a
/// strict multiset superset of the gather.
#[test]
fn the_mutable_walk_covers_the_holders_the_value_gather_omits() {
    // (state name, the family expected to hold the marker, the marker's source expression)
    let expectations: [(&str, RootFamily, &str); 4] = [
        (
            "persistent-term",
            RootFamily::PersistentTermEntry,
            "{persistent, value}",
        ),
        (
            "trace-match-spec",
            RootFamily::TraceMatchSpec,
            "match_first",
        ),
        ("port-data", RootFamily::PortData, "{port, data}"),
        ("runner-recorded", RootFamily::RunnerResult, "55"),
    ];
    for (state, family, marker) in expectations {
        let mut rt = match state {
            "persistent-term" => persistent_term_state().1,
            "trace-match-spec" => trace_match_spec_state().1,
            "port-data" => port_data_state().1,
            "runner-recorded" => runner_recorded_state().1,
            other => panic!("no builder for state {other}"),
        };
        let marker = rt
            .eval(marker)
            .expect("the marker expression evaluates")
            .raw();
        let gathered = value_gather(&mut rt);
        let walked = all_walk(&mut rt);
        let words: Vec<usize> = walked.iter().map(|(_, word)| *word).collect();
        assert!(
            walked.contains(&(family, marker)),
            "state {state}: family {family:?} must carry the marker word {marker:#x}; it visits \
             {:?}",
            walked.iter().filter(|(f, _)| *f == family).count()
        );
        assert!(
            !gathered.contains(&marker),
            "state {state}: {family:?}'s word must NOT be in the value gather"
        );
        assert_strict_superset(&gathered, &words, state);
    }
}

// ---------------------------------------------------------------------------
// 3. Per-process coverage, and an identity rewrite is inert
// ---------------------------------------------------------------------------

/// Every populated per-process family is visited, and a visitor that rewrites each word to itself
/// changes nothing: neither corpus, and not the mailbox's payload order.
#[test]
fn every_process_family_is_visited_and_an_identity_rewrite_changes_nothing() {
    let (mut rt, pid) = live_process_state();
    let counts = per_family_counts(&mut rt);
    for family in [
        RootFamily::ProcessDict,
        RootFamily::ProcessMailbox,
        RootFamily::ProcessRegisters,
        RootFamily::ProcessSavedArgs,
        RootFamily::TimerDelivery,
    ] {
        assert!(
            counts.get(&family).copied().unwrap_or(0) >= 1,
            "state live-process: {family:?} must be visited at least once, got {counts:?}"
        );
    }
    let payments_before = payloads(&rt, pid);
    let gathered_before = value_gather(&mut rt);
    let walked_before = shared_walk(&mut rt);
    // The identity rewrite: every word is replaced by itself.
    rt.for_each_root_mut(&mut |words| {
        for word in words.iter_mut() {
            *word = *word;
        }
        Ok(())
    })
    .expect("the identity walk succeeds");
    assert_eq!(
        payloads(&rt, pid),
        payments_before,
        "the identity rewrite preserved the mailbox's payload order"
    );
    assert_eq!(
        value_gather(&mut rt),
        gathered_before,
        "the identity rewrite left the value gather unchanged"
    );
    assert_eq!(
        shared_walk(&mut rt),
        walked_before,
        "the identity rewrite left the mutable walk unchanged"
    );
}

// ---------------------------------------------------------------------------
// 4. An `Err` still writes the buffer back
// ---------------------------------------------------------------------------

/// A visitor that rewrites the first word it sees and then fails: the walk returns the error, the
/// rewrite is observable through a **second** walk (the buffer was written back, not dropped), and
/// the mailbox is untouched in count and order.
#[test]
fn a_visitor_error_writes_the_buffer_back_and_returns_the_error() {
    let (mut rt, pid) = live_process_state();
    let replacement = make_small(4242);
    let before = multiset_of_all_words(&mut rt);
    let payments_before = payloads(&rt, pid);
    let count_before = rt
        .processes
        .get(pid)
        .expect("the live process")
        .mailbox()
        .len();
    let mut rewritten: Option<usize> = None;
    let outcome = rt.for_each_root_mut(&mut |words| {
        if rewritten.is_none() {
            let original = words[0];
            words[0] = replacement;
            rewritten = Some(original.raw());
            return Err(beam_common::BeamError::BadArg);
        }
        Ok(())
    });
    assert!(
        outcome.is_err(),
        "the walk returns the visitor's error"
    );
    let original = rewritten.expect("the visitor saw at least one word");
    let after = multiset_of_all_words(&mut rt);
    assert!(
        after.contains(&replacement.raw()),
        "the rewritten word is still stored (the buffer was written back)"
    );
    assert_eq!(
        before.len(),
        after.len(),
        "the walk changed no slice length"
    );
    let (added, removed): (Vec<usize>, Vec<usize>) = (
        after
            .iter()
            .filter(|word| !before.contains(word))
            .copied()
            .collect(),
        before
            .iter()
            .filter(|word| !after.contains(word))
            .copied()
            .collect(),
    );
    assert_eq!(
        (added, removed),
        (vec![replacement.raw()], vec![original]),
        "exactly one word changed"
    );
    assert_eq!(
        rt.processes
            .get(pid)
            .expect("the live process")
            .mailbox()
            .len(),
        count_before,
        "the mailbox keeps its message count"
    );
    assert_eq!(
        payloads(&rt, pid),
        payments_before,
        "the mailbox keeps its payload order"
    );
}

// ---------------------------------------------------------------------------
// 5. Order independence
// ---------------------------------------------------------------------------

/// Two runtimes of the same shape built by different insertion orders visit the same families the
/// same number of times, with the same multiset: a visitor must not depend on hash order.
#[test]
fn the_walk_is_independent_of_hash_iteration_order() {
    let forward = ordered_runtime(&["alpha", "beta", "gamma"]);
    let reverse = ordered_runtime(&["gamma", "beta", "alpha"]);
    let mut forward_rt = forward;
    let mut reverse_rt = reverse;
    assert_eq!(
        per_family_counts(&mut forward_rt),
        per_family_counts(&mut reverse_rt),
        "the per-family visit counts are identical"
    );
    assert_eq!(
        shared_walk(&mut forward_rt),
        shared_walk(&mut reverse_rt),
        "the shared-family multisets are identical"
    );
}

// ---------------------------------------------------------------------------
// Helpers the tests above use
// ---------------------------------------------------------------------------

/// A runtime with one process whose dictionary holds `keys` in the given insertion order.
fn ordered_runtime(keys: &[&str]) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let pid = rt.processes.spawn_default().expect("a live process slot");
    let mut words: Vec<(Eterm, Eterm)> = Vec::new();
    for key in keys {
        let key_term = rt.intern(key);
        words.push((key_term, key_term));
    }
    let process = rt.processes.get_mut(pid).expect("the live process");
    for (key, value) in words {
        process.dict_mut().put(key, value);
    }
    rt
}

/// The mailbox's payload words, in FIFO order.
fn payloads(rt: &Runtime, pid: ProcessId) -> Vec<usize> {
    rt.processes
        .get(pid)
        .expect("the live process")
        .mailbox()
        .iter()
        .map(|message| message.payload().raw())
        .collect()
}

/// Every word the mutable walk visits, as a sorted multiset.
fn multiset_of_all_words(rt: &mut Runtime) -> Vec<usize> {
    let mut words = all_walk(rt)
        .into_iter()
        .map(|(_, word)| word)
        .collect::<Vec<usize>>();
    words.sort_unstable();
    words
}
