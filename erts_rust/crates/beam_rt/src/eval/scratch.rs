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
//! the capacity and pushes the old one onto [`Runtime::eval_heap_retired`].
//! A term on the store must stay valid for the runtime's lifetime, so neither half may *move* a
//! word — but a retired region whose words no reachable term stands in **is** dropped, by
//! [`crate::runtime::reclaim`]'s region-reclaim pass, which is driven from this module's two
//! window sites (see below).
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
//!
//! # The window's two sites are also the region-reclaim pass's sites
//!
//! This module owns the *current* region's `[mark, used)` prefix; the sibling
//! [`crate::runtime::reclaim`] pass owns the other two things a `-eval` run retains — the **retired**
//! regions a growth inside the window created, and the engine arena above the window's mark. It
//! runs at the same two instants, from [`open`], [`boundary`] and [`close`], sharing this window's
//! frontier and pin latches so the two mechanisms can never disagree about which words belong to
//! the run.
//!
//! # A pin is classified, and the classification changes nothing here
//!
//! [`pin`] records a **runtime-lifetime** reason ([`PinKind::Lifetime`]: a term a store still names
//! after the driver returns) and [`pin_transient`] a **machine-in-flight** one
//! ([`PinKind::Transient`]: a nested run's — or a nested `-eval` closure's — applying machine, whose
//! roots are dead by the time the driver returns). Every pin makes the window unresettable, exactly
//! as before: this module's own decisions, counters and refusal reasons are identical for both
//! kinds. The classification exists only so the region-reclaim pass can honour a lifetime pin at the
//! **exit** as well as at the boundary, and a transient one at the boundary alone.
//!
//! The practical consequence for this module's *own* rewind is unchanged and one statement wide: a
//! pin costs one statement's words (a bounded leak, never a dangle). A region this window's growth
//! replaced now costs the pass only its **current-region** half — a mark means something only in the
//! region it was taken in, so a replaced region simply has no current-region prefix left to prove,
//! while the retired chain stays fully reclaimable (its candidates are named by index, not by
//! address).

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
    ///
    /// Read by the store move's decline list (a relocation cannot rewrite a lifetime store's slot,
    /// so a latched lifetime pin declines the move at **both** sites) — see
    /// [`crate::runtime::reclaim`]'s decline list. The whole-region pass itself needs no latch: its
    /// root corpus is the complete host-side one.
    #[allow(dead_code)]
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

    /// The open window's **current-region frontier**, as `(region base, mark)` — the floor a
    /// region-reclaim pass may not go below.
    ///
    /// A mark is only meaningful inside the region it was taken in, so a pass whose own cursor
    /// names a different region cannot use it (and has no current-region candidate either way).
    /// The floor exists because this window's mark can only ever advance — on a refusal it jumps to
    /// the store's frontier — while a pass that reset the region *below* it would leave this window
    /// pointing above the frontier, i.e. its next boundary could prove nothing and would refuse with
    /// "the window's mark stands above the store's frontier". Sharing the mark keeps the two
    /// mechanisms' frontiers identical instead of making them cancel each other out.
    pub(crate) fn current_region(&self) -> Option<(usize, usize)> {
        self.window.map(|window| (window.base, window.mark))
    }
}

/// The address of the store's current region — the base every `mark` is an offset into.
///
/// Delegates to [`region_reclaim::heap_region_base`], the one derivation: it is the bump pointer
/// minus the frontier (`hp - used`), **not**
/// [`beam_alloc::Heap::used_words_mut`] — that slice is empty while the region holds no words, and
/// an empty slice's data pointer is dangling, so it would answer a different "base" for an empty
/// region than for the same region once it holds a word. `hp()` is the region's base plus `used`
/// words in both cases.
fn region_base(runtime: &mut Runtime) -> usize {
    region_reclaim::heap_region_base(&runtime.eval_heap)
}

/// Opens the statement window for one `-eval` driver run.
///
/// Called by [`crate::eval::eval`] immediately before the driver walks the expression. A no-op
/// when a window is already open (a nested `-eval` would then inherit the outer one's window,
/// which is exactly right) or when the mechanism is switched off.
///
/// The open is also the region-reclaim pass's **window open** ([`region_reclaim::open`]): the two
/// share one frontier, because a region-reclaim pass may only ever touch the words written inside
/// one `-eval` driver run.
pub(crate) fn open(runtime: &mut Runtime) {
    if !runtime.eval_scratch.enabled || runtime.eval_scratch.window.is_some() {
        return;
    }
    let base = region_base(runtime);
    let used = runtime.eval_heap.used();
    runtime.eval_scratch.open_window(base, used);
    region_reclaim::open(runtime);
}

/// Closes the statement window when the driver returns: one **exit-site** region-reclaim pass, then
/// the region-reclaim window is closed and the statement window dropped — **without** resetting it.
///
/// # What the exit-site pass is for
///
/// The run's own result — the value the caller now holds — may stand in the window, so the words
/// must stay, and this window's own rewind must not fire. The *retired* regions are a different
/// question: a `compile:file/2` in process is one top-level statement, so its 4.63 GB of replaced
/// regions is untouched by any boundary, and every one of them is dead once the result is the only
/// live term. [`region_reclaim::pass`] answers exactly that, and it declines on its own terms, so
/// nothing here weakens the window's own rewind.
///
/// Dropping the statement window is what makes the *next* `-eval` run open its window **above**
/// this result instead of reusing a stale mark that could free it.
///
/// # Why `env` + `extra` is the complete root set here
///
/// At this instant the driver holds exactly its environment plus the one live term it is about to
/// hand back — `Ok(value)`'s result, or a raise's `reason` and its stacktrace `head`. Those are the
/// caller's `extra`; everything else that can name a store word is enumerated by
/// [`Runtime::gather_off_heap_roots`], exactly as at a boundary. A caller that cannot hand its live
/// term over (a [`Flow::Fatal`](crate::eval::Flow::Fatal) whose `BeamError` may carry a live reason
/// word and exposes no mutable accessor) passes `withheld`, which declines the pass.
pub(crate) fn close(
    runtime: &mut Runtime,
    env: &Env,
    extra: &[Eterm],
    withheld: Option<&'static str>,
) {
    let mut value_roots: Vec<Eterm> = env.values().copied().collect();
    value_roots.extend_from_slice(extra);
    let window = runtime.eval_scratch.state();
    region_reclaim::pass(
        runtime,
        PassInput {
            site: Site::Exit,
            window,
            value_roots: &value_roots,
            withheld,
        },
    );
    region_reclaim::close(runtime);
    runtime.eval_scratch.window = None;
}

/// Latches the open window with a **runtime-lifetime** reason ([`PinKind::Lifetime`]): a term was
/// (or may have been) written onto the store and kept by a store that still names it after the
/// driver returns, so this window must not be reset.
///
/// `O(1)`, and free when no window is open (the words then stand below the next window's mark).
/// Only the **first** reason of each kind is kept, which is the one that explains the decline.
///
/// The latch stops at *this* window. The region-reclaim pass
/// ([`crate::runtime::reclaim`]) does not need it: its root set is the runtime's **complete**
/// host-side corpus ([`Runtime::for_each_root_mut`] plus a raise's live reason), gathered at these
/// same two non-running sites, so a region holding a `stabilize_term` word is retained because the
/// descent *finds* that word rather than because a latch forbade looking. The
/// [`PinKind::Lifetime`] classification is kept for the pass's **move** (a relocation cannot
/// rewrite every lifetime store's slot, so it declines on this reason) — see
/// [`crate::runtime::reclaim`]'s decline list.
pub(crate) fn pin(runtime: &mut Runtime, reason: &'static str) {
    runtime.eval_scratch.pin(reason, PinKind::Lifetime);
}
/// Latches the open window with a **transient** reason ([`PinKind::Transient`]): a holder that
/// exists only while something executes — a nested run's machine, a nested `-eval` closure's
/// applying machine — may name a word written inside the window.
///
/// This module's own behaviour is identical to [`pin`]'s (any pin makes the window unresettable);
/// the classification exists so a region-reclaim pass knows the holder is dead by the time the
/// driver returns and may act on the exit site.
pub(crate) fn pin_transient(runtime: &mut Runtime, reason: &'static str) {
    runtime.eval_scratch.pin(reason, PinKind::Transient);
}

/// The boundary **before** a top-level statement: reclaims the previous statement's dead copies,
/// then runs a **boundary-site** region-reclaim pass.
///
/// `env_roots` is the evaluator's live set — every value its [`Env`] binds. Everything else that
/// can name a store word is enumerated by [`Runtime::gather_off_heap_roots`]. See the module docs
/// for why each refusal leaves the store exactly as it was.
///
/// # Why the pass runs even when the rewind was refused
///
/// [`rewind_window`] answers only about the **current** region's `[mark, used)` prefix: it refuses
/// on a replaced region, on a mark above the frontier, and on a pinned window. None of those says
/// anything about the **retired** regions this window created — a compile pins the window on its
/// first store-writing nested run and replaces its region repeatedly, so the very case with the
/// biggest retention is exactly the one the rewind must refuse and the pass can still act on. The
/// pass therefore has its own decline list ([`region_reclaim::refusal`]) and is not gated on the
/// rewind's verdict.
pub(crate) fn boundary(runtime: &mut Runtime, env_roots: &[Eterm]) {
    if !runtime.eval_scratch.enabled {
        return;
    }
    rewind_window(runtime, env_roots);
    let window = runtime.eval_scratch.state();
    region_reclaim::pass(
        runtime,
        PassInput {
            site: Site::Boundary,
            window,
            value_roots: env_roots,
            withheld: None,
        },
    );
}

/// The statement window's own **current-region rewind** — the pre-existing mechanism, unchanged:
/// every refusal, counter and reason here is byte-identical to what it was before the
/// region-reclaim pass existed.
fn rewind_window(runtime: &mut Runtime, env_roots: &[Eterm]) {
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
        ranges: [(base + window.mark * width, base + used * width)],
        hits: [false],
    };
    if let Walk::Declined(reason) = reclaim::walk(&roots, &mut probe) {
        runtime.eval_scratch.decline(base, used, reason);
        return;
    }
    if probe.hits[0] {
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

/// The descent's sink for the general region question: **which of these address ranges holds a
/// reachable term**?
///
/// One probe, two callers: this module's statement window asks about one range
/// ([`WindowProbe`]), and [`crate::runtime::reclaim`] asks about every range a pass may touch — the
/// store's current-region prefix, each retired region the window created, and every arena region
/// at or above the window's mark. One walk answers all of them, so a pass costs one descent rather
/// than one per region.
///
/// It **never dereferences a word** — a reached term's pointer *value* is compared with each
/// range's half-open bounds — so it needs no `unsafe` and cannot fault. The walk's roots are real
/// terms ([`Runtime::gather_off_heap_roots`] plus the caller's live set), so the guards below only
/// reject immediates, literals and bare header words.
pub(crate) struct RangeProbe<'a> {
    /// The half-open byte ranges `[lo, hi)` to test, in the caller's order.
    ranges: &'a [(usize, usize)],
    /// One flag per range, set when a reached term's address falls inside it.
    hits: &'a mut [bool],
}

impl<'a> RangeProbe<'a> {
    /// A probe over `ranges` recording into `hits` — one flag per range, in the same order.
    pub(crate) fn new(ranges: &'a [(usize, usize)], hits: &'a mut [bool]) -> RangeProbe<'a> {
        debug_assert_eq!(
            ranges.len(),
            hits.len(),
            "every range needs exactly one hit flag"
        );
        RangeProbe { ranges, hits }
    }
}

impl Reach for RangeProbe<'_> {
    fn word(&mut self, term: Eterm) {
        if beam_term::literal::is_literal(term)
            || !beam_term::tag::is_term_word(term)
            || !(beam_term::boxed::is_boxed_term(term) || beam_term::prelude::is_list(term))
        {
            return;
        }
        let address = beam_term::list::ptr_val(term) as usize;
        if address == 0 {
            return;
        }
        for (range, hit) in self.ranges.iter().zip(self.hits.iter_mut()) {
            if address >= range.0 && address < range.1 {
                *hit = true;
            }
        }
    }

    /// The whole question is answered once every range has its answer, so the descent may stop
    /// there. A probe over no ranges is done immediately (nothing was asked).
    fn done(&self) -> bool {
        self.hits.iter().all(|hit| *hit)
    }
}

/// This module's question as a **one-range** use of [`RangeProbe`]: does any reachable term stand
/// inside the statement window?
///
/// The single flag is set once and the descent's [`Reach::done`] then reports the answer.
struct WindowProbe {
    /// The window's half-open byte range `[lo, hi)`.
    ranges: [(usize, usize); 1],
    /// Set once a term inside the window was reached: the window is live, so it must not be reset.
    hits: [bool; 1],
}

impl Reach for WindowProbe {
    fn word(&mut self, term: Eterm) {
        // Field-level borrows keep this legal without a temporary range array.
        RangeProbe::new(&self.ranges, &mut self.hits).word(term);
    }

    /// The question is answered by the first hit, so the descent may stop there.
    fn done(&self) -> bool {
        self.hits[0]
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
