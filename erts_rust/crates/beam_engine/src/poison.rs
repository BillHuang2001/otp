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

//! A **diagnostic** from-space poison detector for the GC root-set audit.
//!
//! A copying collection evacuates the live words of its from-space (the young nursery,
//! plus the mature generation for a fullsweep) and abandons every address it did not
//! copy. A holder the collector does not publish — a machine field outside the closed
//! [`RootSet`](crate::context::RootSet) shape, a host Rust local, a nested machine's
//! register seeded from an argument — keeps pointing into that abandoned region, and a
//! later allocation reusing the address turns the stale word into a wrong term.
//!
//! The detector makes that exact condition observable: a host that runs a collection
//! registers the **abandoned from-space range** ([`record_abandoned`]) and then arms the
//! window ([`arm`]); every instrumented term write/read calls [`check`] (or
//! [`check_slice`]), and the first word whose boxed target lies in an abandoned range is
//! reported once, with its holder name and a captured backtrace.
//!
//! # Scope
//!
//! This is a **debugging aid, off by default**: [`enabled`] reads `BEAM_RT_POISON` once
//! and caches it, and every hook is a single cached-bool test plus a range scan when it
//! is off. It never changes any behaviour the runtime depends on — it only observes.
//!
//! The state is **per thread** (`thread_local`): the scheduler is single-threaded per
//! process, and a parallel test harness must not let one thread's abandoned ranges
//! decide another thread's verdict.

use std::cell::RefCell;

use beam_term::Eterm;

/// The detector's per-thread state.
#[derive(Default)]
struct State {
    /// The cached answer of the `BEAM_RT_POISON` environment test.
    enabled: Option<bool>,
    /// The abandoned from-space ranges (`[lo, hi)` byte addresses), as registered by the
    /// host that ran the collection.
    abandoned: Vec<(usize, usize)>,
    /// Whether the ranges describe a **live** collection window (see [`arm`]).
    armed: bool,
    /// The first reported word (so a verdict is printed exactly once), or `None`.
    reported: Option<usize>,
}

thread_local! {
    /// This thread's detector state.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Whether the detector is on (`BEAM_RT_POISON` set to a non-empty, non-`0` value).
///
/// The environment is read once and cached, so a hot hook pays a `bool` load.
pub fn enabled() -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(value) = state.enabled {
            return value;
        }
        let on = std::env::var_os("BEAM_RT_POISON").is_some_and(|value| !value.is_empty());
        state.enabled = Some(on);
        on
    })
}

/// Registers one abandoned from-space range (`[lo, hi)` **byte** addresses) and arms the
/// window: the subsequent [`check`] calls treat a boxed target inside the range as stale.
///
/// A host calls this once per collection, with the range the collector evacuated (the
/// young nursery's handed-out region), and calls [`clear`] when the collection's window
/// ends.
pub fn record_abandoned(lo: usize, hi: usize) {
    if !enabled() || hi <= lo {
        return;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.abandoned.push((lo, hi));
        state.armed = true;
    });
}

/// Clears every registered range and disarms the window.
pub fn clear() {
    if !enabled() {
        return;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.abandoned.clear();
        state.armed = false;
    });
}

/// The abandoned range a boxed/list `word` points into, or `None` when the word is an
/// immediate, a literal, or a pointer outside every registered range.
///
/// A registered range is consulted **only while the window is armed**, so a range left
/// behind by an old collection can never misjudge a later term.
pub fn abandoned_target(word: Eterm) -> Option<usize> {
    if !enabled() {
        return None;
    }
    if !beam_term::prelude::is_boxed(word) && !beam_term::prelude::is_list(word) {
        return None;
    }
    if beam_term::literal::is_literal(word) {
        return None;
    }
    let target = beam_term::list::ptr_val(word) as usize;
    STATE.with(|state| {
        let state = state.borrow();
        if !state.armed {
            return None;
        }
        state
            .abandoned
            .iter()
            .any(|&(lo, hi)| lo <= target && target < hi)
            .then_some(target)
    })
}

/// The number of registered ranges (for a host's own window bookkeeping).
pub fn range_count() -> usize {
    STATE.with(|state| state.borrow().abandoned.len())
}

/// Reports `word` as a **stale** term held by `holder` if its boxed target lies in a
/// registered abandoned range — printing the holder, the word, the target and a captured
/// backtrace, **once** per process.
///
/// `what` names the operation ("write"/"read"/"arg") so a report says where the word was
/// observed.
pub fn check(what: &str, holder: &str, word: Eterm) {
    let Some(target) = abandoned_target(word) else {
        return;
    };
    if reported() {
        return;
    }
    let ranges = STATE.with(|state| state.borrow().abandoned.clone());
    let backtrace = std::backtrace::Backtrace::force_capture();
    eprintln!(
        "POISON: {what} of a stale term by `{holder}`: word={word:?} -> target={target:#x} \
         (inside an abandoned from-space range)\nabandoned ranges: {ranges:#x?}\n{backtrace}"
    );
}

/// [`check`] over a slice of terms (a root set, a register file, a call's arguments).
pub fn check_slice(what: &str, holder: &str, words: &[Eterm]) {
    if !enabled() {
        return;
    }
    for &word in words {
        check(what, holder, word);
    }
}

/// A snapshot of a holder's words across a collection window (empty when disabled).
pub fn snapshot(words: &[Eterm]) -> Vec<Eterm> {
    if !enabled() {
        return Vec::new();
    }
    words.to_vec()
}

/// Reports the first word that is **unchanged** across a collection window yet still points
/// into a registered abandoned range — i.e. a holder the collection failed to relocate.
///
/// This is the precise provenance test: a *correct* holder's relocated word necessarily
/// **changed** (the collector rewrote every from-space pointer it saw), so a from-space word
/// that survived the window unchanged can only be one the collector never published.
pub fn audit_unchanged(what: &str, holder: &str, before: &[Eterm], after: &[Eterm]) {
    if !enabled() || before.len() != after.len() {
        return;
    }
    for (index, (old, new)) in before.iter().zip(after.iter()).enumerate() {
        if old == new && abandoned_target(*new).is_some() {
            check(what, &format!("{holder}[{index}]"), *new);
            return;
        }
    }
}

/// Whether a report has already been printed on this thread.
fn reported() -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.reported.is_some() {
            return true;
        }
        state.reported = Some(1);
        false
    })
}
