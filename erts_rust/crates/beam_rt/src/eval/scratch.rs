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

//! The `-eval` route's **statement window** on the runtime term store
//! ([`Runtime::eval_heap`]).
//!
//! # The growth this bounds
//!
//! [`Runtime::eval_heap`] is a pointer-stable [`beam_alloc::Heap`]: a bump arena whose `used`
//! frontier only ever moves forward, and whose *replacement* growth
//! ([`Runtime::grow_eval_heap`], taken when a copy does not fit) allocates a fresh region of twice
//! the capacity and pushes the old one onto [`Runtime::eval_heap_retired`], which is never freed.
//! Both halves are deliberate — a term on the store must stay valid for the runtime's lifetime —
//! but neither *reclaims*: the words a `-eval` run's dead copies occupy are retained until the
//! process exits, so a long run's memory is the **total volume it ever copied**, not its live set.
//!
//! This module reclaims the dead part at the one point where the live set is both small and
//! nameable: the boundary **between the top-level statements** of a `-eval` expression.
//!
//! # Why a statement boundary, and why *before* the next statement
//!
//! A comma sequence's semantics are exactly what makes this sound: the value of every statement
//! except the last is **discarded** by the language, so at the instant the next statement begins
//! the only terms the expression still holds are the ones the evaluator's [`Env`] binds — plus
//! whatever a runtime-lifetime store kept (below). Everything else the previous statement copied
//! onto the store is dead, and dead is what this module reclaims. Taking the boundary *before*
//! the statement (not after it) is load-bearing: the statement's own value is by construction a
//! term the statement just built, so a boundary that also saw that value could never reclaim the
//! very copy it wants to.
//!
//! The reducible shape is therefore `_ = <build>, _ = <build>, …`: with a window per statement,
//! the store's frontier after each statement returns to the baseline instead of marching
//! forward once per build. A single giant statement is inherently retained (its value is the
//! result), and a statement whose value is bound into the environment is retained too — both by
//! the language, not by this mechanism.
//!
//! # The three parts of the soundness argument
//!
//! 1. **A reset must never free a word a live term names.** The window `[mark, used)` of the
//!    current region is reclaimed only after a reachability descent from the complete root
//!    enumeration ([`Runtime::gather_off_heap_roots`] plus the evaluator's environment) found
//!    **no** word inside it. That is the same descent, root enumeration and refusal rule the
//!    escaped-payload prune rests on ([`crate::eval::reclaim`]), reused rather than re-stated:
//!    [`reclaim::walk`] with a [`WindowProbe`] sink. A descent that cannot complete — an
//!    exhausted visit budget, an unknown boxed header — **declines**, and a decline retains
//!    every word.
//! 2. **A holder the descent cannot see latches the window.** Every writer whose term may be
//!    retained outside the enumerated root families calls [`pin`], which makes the window
//!    unresettable and merely advances its mark. The writers are classified below; the ones that
//!    pin are the runtime-lifetime stores, a nested evaluator's driver, and a nested run's
//!    machine **when that run wrote on the store** — a nested run that copied nothing leaves the
//!    window resettable, which is why the window outlives an ordinary nested call and still
//!    bounds the statements around it. A pinned window is a bounded leak of one statement, never
//!    a dangle.
//! 3. **Doubt ⇒ keep.** Nothing here has a fast path that skips a check: the cheap state is
//!    "no window", "window pinned", or "the window is smaller than [`MIN_WINDOW_WORDS`]", and all
//!    three leave every word exactly where it was.
//!
//! # Writer classification (the enumeration this mechanism depends on)
//!
//! Every writer of [`Runtime::eval_heap`] and what this module does about it:
//!
//! | writer | retention | treatment |
//! |---|---|---|
//! | [`RuntimeContext::move_result_into_runtime_store`](crate::context::RuntimeContext) | the BIF's container result, handed straight back to the code that called it | transient — the descent reaches it through the environment, or it died with the statement |
//! | the store's growth retry in that path | the retried copy | transient (same term) |
//! | [`Evaluator::materialize_bits`](crate::eval::Evaluator) | a binary/bitstring literal of the statement being evaluated | transient |
//! | `src/native/**`'s `beam_codec::term::make_binary(self.eval_heap(), …)` | a BIF's own binary result (a file/prim/code reply, an `erts_internal` image) | transient — a message reply is copied into the receiver's mailbox, which *is* an enumerated root family |
//! | [`Runtime::stabilize_term`] / [`Runtime::stabilize_term_with`] | **runtime-lifetime**: the runner's recorded outcomes, `try_stabilize_escaped*` (pending spawns, the native message/function stores, the `RuntimeHooks` store, a `persistent_term` value, a pending timer's message, the exit reason) | **pin** |
//! | `src/port/completion.rs`'s `decode_caller_tag` | a decoded port-message tag kept in runtime-level port state | **pin** |
//! | [`Runtime::grow_eval_heap`] | nothing — it *replaces* the region | a stale `mark` is a per-region offset, so the window declines whenever the region's base moves |
//! | [`Runtime::adopt_escaped_binaries`] | off-heap payload handles, not region words | irrelevant here |
//!
//! and the *holders* (a family can hold a region word without writing one):
//!
//! | holder | treatment |
//! |---|---|
//! | the evaluator's [`Env`] | a root of the descent |
//! | `Runtime::eval_closures[..].env` | an enumerated root family ([`Runtime::gather_off_heap_roots`]) |
//! | every live process's dictionary, mailbox, registers, saved arguments and signals | enumerated root families |
//! | pending timer deliveries, `seq_trace` tokens, pending spawns, an engine's arena-stable arguments and a parked machine's root set | enumerated root families |
//! | a nested run's machine (`run_export_blocking` / `run_label_blocking`) — its registers are live and this module cannot enumerate them | **latches at the call site when the run wrote on the store** (`Runtime::pin_window_if_store_written`: a run that copied nothing leaves the window resettable) |
//! | a nested [`Evaluator`] applied by a loaded module (`apply_eval_closure`), whose copies are held by the *module's* machine (e.g. `lists:map/2`'s accumulator) | **pin at the call site** (unconditional: the closure's own terms are the machine's from the first instruction) |
//!
//! # What a reset does to the other readers of the region
//!
//! [`Runtime::for_each_off_heap_region`] — and through it the escaped-payload prune's word scan
//! ([`crate::eval::reclaim`]) and the NIF-resource prune's
//! ([`crate::eval::reclaim::resource`]) — is handed `eval_heap`'s written prefix. A reset only
//! ever *lowers* `used` in the current region, and the words it drops are the ones the descent
//! proved dead, so those readers see strictly fewer — and still live — words. The retired
//! regions are untouched: a reset never frees a region.
//!
//! # Cost
//!
//! A boundary costs one `Option` test and one `usize` subtraction while the window is closed,
//! pinned, or below [`MIN_WINDOW_WORDS`]. Only a window worth reclaiming pays the descent, and
//! the descent stops at its first hit (`Reach::done`).

use std::mem::size_of;

use beam_term::Eterm;

use super::reclaim::{self, Reach, Walk};
use super::{Env, Evaluator};
use crate::runtime::reclaim::{self as region_reclaim, PassInput, Site};
use crate::runtime::Runtime;
/// The term store's region figures and the statement window's counters, as
/// [`Runtime::eval_region_stats`] publishes them.
///
/// One value, read by a measurement and by the pins in `src/eval/scratch/tests.rs`: the claim the
/// window makes is that `used` stops growing with the number of discarded statements while
/// `resets` grows with it, and that with the mechanism switched off
/// ([`Runtime::set_eval_scratch_enabled`]) the same expression is linear again.
#[derive(Debug, Clone, Copy)]
pub struct EvalRegionStats {
    /// The current region's written prefix, in words ([`Runtime::eval_heap`]).
    pub used: usize,
    /// The current region's capacity, in words.
    pub capacity: usize,
    /// How many regions the store has replaced and retained
    /// ([`Runtime::eval_heap_retired`]).
    pub retired_regions: usize,
    /// The retained regions' summed capacity, in words.
    pub retired_capacity_words: usize,
    /// How many windows were opened.
    pub opens: u64,
    /// How many windows were actually reset.
    pub resets: u64,
    /// How many windows a refusal declined.
    pub declines: u64,
    /// The most recent refusal's reason.
    pub last_decline: Option<&'static str>,
    /// The words the resets reclaimed, summed.
    pub words_reclaimed: u64,
    /// The **region reclamation** counters
    /// ([`RegionReclaimStats`](crate::runtime::RegionReclaimStats)) of the same runs — the
    /// [`crate::runtime::reclaim`] pass, which drops whole-dead store regions and rewinds the
    /// engine arena at the two window sites. Independent of this window's own rewind: a window
    /// whose region was replaced still leaves a reclaimable retired chain.
    pub reclaim: crate::runtime::RegionReclaimStats,
}
/// The smallest window worth a reachability descent, in term words (8 KiB).
///
/// Below it a boundary does nothing at all: the descent costs `O(live set)` and a statement that
/// copied less than this would not repay it. The window then keeps growing until it is worth a
/// walk, so nothing is *lost* — only deferred.
const MIN_WINDOW_WORDS: usize = 1024;

/// One open **statement window**: the part of the store the statement (or statements) since the
/// mark have written.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// The address of the region's first word — the identity that makes [`Window::mark`] mean
    /// something. [`Runtime::grow_eval_heap`] replaces the region, and a mark is only valid in the
    /// region it was taken in.
    base: usize,
    /// The region's `used` frontier at the window's start, in words.
    mark: usize,
    /// The first reason this window must not be reset, of **either** kind, and why a pinned window
    /// merely advances.
    pinned: Option<&'static str>,
    /// The first reason a **runtime-lifetime** store latched this window ([`pin`]).
    ///
    /// A lifetime pin and a transient one are not the same claim (see [`PinKind`]): a lifetime
    /// store still names its word after the driver returns, so it outlives the window, while a
    /// machine-in-flight pin does not.
    lifetime_pin: Option<&'static str>,
}

/// Why a window was latched, and whether the reason outlives the window.
///
/// The distinction is [`crate::runtime::reclaim`]'s one input the statement window owns; it never
/// changes what *this* module does (any pin makes the window unresettable, exactly as before).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinKind {
    /// A store that outlives the `-eval` driver run keeps a store word —
    /// [`Runtime::stabilize_term`]/[`Runtime::stabilize_term_with`] and
    /// [`crate::port::completion`]'s decoded caller tag. Its word is still named after the driver
    /// returns, so a region holding it may not be dropped at **either** site.
    Lifetime,
    /// A holder that exists only while something is executing — a nested run's machine, or a
    /// nested `-eval` closure's applying machine. Its roots are dead by the time the driver
    /// returns, so this pin is only a refusal at the **boundary**.
    Transient,
}

/// What an open statement window reports to a region-reclaim pass
/// ([`crate::runtime::reclaim::pass`]).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WindowState {
    /// Whether a window is open at all — the pass has no site without one.
    pub(crate) open: bool,
    /// The first reason the window was pinned, of either kind.
    pub(crate) pinned: Option<&'static str>,
    /// The first **runtime-lifetime** reason, if one latched.
    pub(crate) lifetime_pinned: Option<&'static str>,
}
/// The `-eval` route's statement-window bookkeeping on [`Runtime::eval_heap`].
///
/// Always present on the runtime; `window` is `Some` only between [`open`] and [`close`], i.e.
/// for the duration of one `-eval` driver run. Every field's meaning is in the module docs.
#[derive(Debug, Clone)]
pub(crate) struct ScratchState {
    /// The open window, if any.
    window: Option<Window>,
    /// The diagnostic kill switch ([`Runtime::set_eval_scratch_enabled`]); while it is off no
    /// window opens and no state is recorded, so a measurement can reproduce the pre-fix growth
    /// on the same binary.
    enabled: bool,
    /// How many windows were opened.
    pub(crate) opens: u64,
    /// How many windows were actually reset.
    pub(crate) resets: u64,
    /// How many windows were declined because soundness could not be shown.
    pub(crate) declines: u64,
    /// The reason of the most recent decline.
    pub(crate) last_decline: Option<&'static str>,
    /// The words the resets reclaimed (a sum over every reset).
    pub(crate) words_reclaimed: u64,
}

impl Default for ScratchState {
    fn default() -> Self {
        Self {
            window: None,
            enabled: true,
            opens: 0,
            resets: 0,
            declines: 0,
            last_decline: None,
            words_reclaimed: 0,
        }
    }
}

impl ScratchState {
    /// The diagnostic kill switch: while it is off no window opens, no boundary runs, and the
    /// open one (if any) is dropped — so the store behaves exactly as it did before this module
    /// existed. See [`Runtime::set_eval_scratch_enabled`].
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.window = None;
        }
    }

    /// Opens a window at the region's current frontier (and counts it).
    fn open_window(&mut self, base: usize, used: usize) {
        self.window = Some(Window {
            base,
            mark: used,
            pinned: None,
            lifetime_pin: None,
        });
        self.opens += 1;
    }

    /// Declines this window: records the reason and advances the mark to the frontier, so the
    /// next window starts clean and the words kept are exactly one statement's worth.
    fn decline(&mut self, base: usize, used: usize, reason: &'static str) {
        self.declines += 1;
        self.last_decline = Some(reason);
        self.window = Some(Window {
            base,
            mark: used,
            pinned: None,
            lifetime_pin: None,
        });
    }

    /// Records a completed reset of `words` words; the mark stays where it is, because the
    /// frontier is now exactly the mark again.
    fn note_reset(&mut self, words: usize) {
        self.resets += 1;
        self.words_reclaimed += words as u64;
        if let Some(window) = &mut self.window {
            window.pinned = None;
            window.lifetime_pin = None;
        }
    }
    /// Latches the window with `reason`, of the given [`PinKind`]. Only the **first** reason of
    /// each kind is kept: the first overall explains the window's decline, and the first lifetime
    /// one explains why a region that may hold it cannot be dropped even after the driver returns.
    fn pin(&mut self, reason: &'static str, kind: PinKind) {
        if let Some(window) = &mut self.window {
            if window.pinned.is_none() {
                window.pinned = Some(reason);
            }
            if kind == PinKind::Lifetime && window.lifetime_pin.is_none() {
                window.lifetime_pin = Some(reason);
            }
        }
    }
    /// What a region-reclaim pass needs to know about this window.
    pub(crate) fn state(&self) -> WindowState {
        match &self.window {
            Some(window) => WindowState {
                open: true,
                pinned: window.pinned,
                lifetime_pinned: window.lifetime_pin,
            },
            None => WindowState::default(),
        }
    }
}

/// The address of the store's current region — the base every `mark` is an offset into.
///
/// Derived from the bump pointer and the frontier (`hp - used`), **not** from
/// [`beam_alloc::Heap::used_words_mut`]: that slice is empty while the region holds no words, and
/// an empty slice's data pointer is dangling — so it would answer a different "base" for an empty
/// region than for the same region once it holds a word. `hp()` is the region's base plus `used`
/// words in both cases.
fn region_base(runtime: &mut Runtime) -> usize {
    let used = runtime.eval_heap.used();
    (runtime.eval_heap.hp() as usize).saturating_sub(used * size_of::<Eterm>())
}

/// Opens the statement window for one `-eval` driver run.
///
/// Called by [`crate::eval::eval`] immediately before the driver walks the expression. A no-op
/// when a window is already open (a nested `-eval` would then inherit the outer one's window,
/// which is exactly right) or when the mechanism is switched off.
pub(crate) fn open(runtime: &mut Runtime) {
    if !runtime.eval_scratch.enabled || runtime.eval_scratch.window.is_some() {
        return;
    }
    let base = region_base(runtime);
    let used = runtime.eval_heap.used();
    runtime.eval_scratch.open_window(base, used);
}

/// Closes the window when the driver returns, **without** resetting it.
///
/// The run's own result — the value the caller now holds — may stand in the window, so the words
/// must stay. Dropping the window is what makes the *next* `-eval` run open its window **above**
/// this result instead of reusing a stale mark that could free it.
pub(crate) fn close(runtime: &mut Runtime) {
    runtime.eval_scratch.window = None;
}

/// Latches the open window: a term was (or may have been) written onto the store and kept by a
/// holder this module cannot enumerate, so the window must not be reset.
///
/// `O(1)`, and free when no window is open (the words then stand below the next window's mark).
/// Only the **first** reason is kept, which is the one that explains the decline.
pub(crate) fn pin(runtime: &mut Runtime, reason: &'static str) {
    if let Some(window) = &mut runtime.eval_scratch.window {
        if window.pinned.is_none() {
            window.pinned = Some(reason);
        }
    }
}

/// The boundary **before** a top-level statement: reclaims the previous statement's dead copies.
///
/// `env_roots` is the evaluator's live set — every value its [`Env`] binds. Everything else that
/// can name a store word is enumerated by [`Runtime::gather_off_heap_roots`]. See the module docs
/// for why each refusal leaves the store exactly as it was.
pub(crate) fn boundary(runtime: &mut Runtime, env_roots: &[Eterm]) {
    if !runtime.eval_scratch.enabled {
        return;
    }
    let base = region_base(runtime);
    let used = runtime.eval_heap.used();
    let Some(window) = runtime.eval_scratch.window else {
        runtime.eval_scratch.open_window(base, used);
        return;
    };
    // The refusals that need no descent: a replaced region (a mark only means something in the
    // region it was taken in), a mark above the frontier, and a window a writer already pinned.
    let refusal = if window.base != base {
        Some("the store's region was replaced inside the window")
    } else if window.mark > used {
        Some("the window's mark stands above the store's frontier")
    } else {
        window.pinned
    };
    if let Some(reason) = refusal {
        runtime.eval_scratch.decline(base, used, reason);
        return;
    }
    let words = used - window.mark;
    if words < MIN_WINDOW_WORDS {
        // Not worth a descent: leave the mark where it is so the window keeps growing.
        return;
    }
    // The complete root enumeration: the evaluator's environment plus every named live set
    // (`Runtime::gather_off_heap_roots`).
    let mut roots: Vec<Eterm> = Vec::with_capacity(env_roots.len() + 64);
    roots.extend_from_slice(env_roots);
    runtime.gather_off_heap_roots(&mut roots);
    let width = size_of::<Eterm>();
    let mut probe = WindowProbe {
        lo: base + window.mark * width,
        hi: base + used * width,
        hit: false,
    };
    if let Walk::Declined(reason) = reclaim::walk(&roots, &mut probe) {
        runtime.eval_scratch.decline(base, used, reason);
        return;
    }
    if probe.hit {
        runtime
            .eval_scratch
            .decline(base, used, "a live term stands inside the window");
        return;
    }
    if runtime.eval_heap.reset_to(window.mark).is_err() {
        // The mark was checked against the frontier above, so this cannot happen today — but a
        // refused rewind must keep every word, never assume it succeeded.
        runtime
            .eval_scratch
            .decline(base, used, "the store refused to rewind to the window's mark");
        return;
    }
    runtime.eval_scratch.note_reset(words);
}

/// The descent's sink for this module's question: **does any reachable term stand inside the
/// window**?
///
/// It never dereferences a word — a reached term's pointer *value* is compared with the window's
/// half-open address range — so it needs no `unsafe` and cannot fault. The walk's roots are real
/// terms ([`Runtime::gather_off_heap_roots`] plus the evaluator's environment), so the guards here
/// only reject immediates, literals and bare header words.
struct WindowProbe {
    /// The window's first byte address (inclusive).
    lo: usize,
    /// Its end (exclusive).
    hi: usize,
    /// Set once a term inside the window was reached: the window is live, so it must not be reset.
    hit: bool,
}

impl Reach for WindowProbe {
    fn word(&mut self, term: Eterm) {
        if self.hit
            || beam_term::literal::is_literal(term)
            || !beam_term::tag::is_term_word(term)
            || !(beam_term::boxed::is_boxed_term(term) || beam_term::prelude::is_list(term))
        {
            return;
        }
        let address = beam_term::list::ptr_val(term) as usize;
        if address != 0 && address >= self.lo && address < self.hi {
            self.hit = true;
        }
    }

    /// The question is answered by the first hit, so the descent may stop there.
    fn done(&self) -> bool {
        self.hit
    }
}

impl Evaluator<'_, '_> {
    /// The boundary for one top-level statement: reclaims the store words the previous statement
    /// copied and dropped.
    ///
    /// Gated exactly like [`Evaluator::prune_escaped`] — only the `-eval` **driver** enables it
    /// (`prune`, which [`crate::eval::eval`] sets) and only at depth 0 — because a nested
    /// evaluator's caller holds terms this module cannot enumerate (see the module docs' holder
    /// table; such a nested evaluator also pins the window when it is applied,
    /// [`apply_eval_closure`](crate::eval::apply_eval_closure)).
    pub(super) fn reset_scratch(&mut self, env: &Env) {
        if !self.prune || self.depth != 0 {
            return;
        }
        let roots: Vec<Eterm> = env.values().copied().collect();
        boundary(self.ctx.runtime, &roots);
    }
}

#[cfg(test)]
mod tests;
