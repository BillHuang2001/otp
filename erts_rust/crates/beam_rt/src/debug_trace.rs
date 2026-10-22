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

//! The runtime's **env-gated debug trace**: a diagnostic channel for the boot path and the
//! scheduler pass.
//!
//! Set `BEAM_RT_BOOT_TRACE` to any value to turn it on; every line goes to **stderr**,
//! prefixed `[rt-trace]`. With the variable unset the module costs one `env::var_os` per
//! *call site that asks* — never an unconditional write — so a normal run is silent.
//!
//! Two sibling channels live here because they are the same kind of thing and must never be
//! confused with a contract: `BEAM_RT_DEBUG_DIST` (`[rt-dist]`, the two distribution halves and
//! the peer-loss drain) and `BEAM_RT_MEM_TRACE` (`[rt-mem]`, the one-line snapshot of the
//! runtime's four never-freed storage regions, printed after the boot and on the way out —
//! [`crate::runtime::RetentionStats`]).
//!
//! Two more sit beside them: `BEAM_RT_GC_TRACE` (`[rt-heap]`, one line per copy-out nursery
//! growth) and `BEAM_RT_ARENA_TRACE` (`[rt-arena]`, the arena allocation attribution census —
//! the size histogram, the largest single requests and the nested-run window split
//! [`crate::runtime::arena_census`] records).
//!
//! It exists because the boot sequence's failure modes (a process that is never enqueued, a
//! scheduler pass that drains with work outstanding) are not observable from the outside:
//! the trace records what the scheduler and the boot drives actually did, rather than what
//! they were meant to do. It is a **diagnostic**, not a contract — no behaviour depends on
//! it.

use std::fmt;
use std::sync::OnceLock;

/// Whether the debug trace is enabled (`BEAM_RT_BOOT_TRACE` present).
///
/// Resolved **once** per OS process: a trace switch is read from the environment at most
/// once, so a hot path pays one relaxed load after the first call.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_BOOT_TRACE").is_some())
}

/// Writes one trace line to stderr when [`enabled`]; a no-op otherwise.
pub(crate) fn trace(args: fmt::Arguments<'_>) {
    if enabled() {
        eprintln!("[rt-trace] {args}");
    }
}

/// Whether the **distribution diagnostic** trace is on (`BEAM_RT_DEBUG_DIST` present).
///
/// A channel of its own rather than a second use of [`enabled`]: a live two-VM run's boot
/// trace is thousands of lines, and what a peer-loss investigation needs is the handful the
/// link halves and the peer-loss drain emit. Resolved once per OS process, like `enabled`.
pub(crate) fn dist_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_DEBUG_DIST").is_some())
}

/// Writes one distribution diagnostic line to stderr when [`dist_enabled`]; a no-op
/// otherwise. A diagnostic only — no behaviour depends on it.
pub(crate) fn dist(args: fmt::Arguments<'_>) {
    if dist_enabled() {
        eprintln!("[rt-dist] {args}");
    }
}

/// Whether the **retention** diagnostic is on (`BEAM_RT_MEM_TRACE` present).
///
/// A third channel of its own: the boot and distribution traces both write on paths this one
/// never touches, and the figure it reports is a single line per point — the snapshot of the
/// runtime's four never-freed storage regions ([`crate::runtime::RetentionStats`]). Resolved
/// once per OS process, like `enabled`.
pub(crate) fn mem_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_MEM_TRACE").is_some())
}

/// Writes one retention line to stderr when [`mem_enabled`]; a no-op otherwise.
///
/// A diagnostic only — no behaviour depends on it, and with the variable unset a call site
/// costs the one cached `OnceLock` load of [`mem_enabled`].
pub(crate) fn mem(args: fmt::Arguments<'_>) {
    if mem_enabled() {
        eprintln!("[rt-mem] {args}");
    }
}
/// Whether the **per-process heap** diagnostic is on (`BEAM_RT_GC_TRACE` present).
///
/// A fourth channel of its own, for the one question a whole-run memory figure cannot answer:
/// *which* process's nursery is being grown, how full it is, and whether the collector is
/// carrying retirement debt at that moment. The copy-out growth
/// ([`crate::context::RuntimeContext`]'s `hint_grow_gate`) is a **no-collect** seam, so a
/// chain that only ever grows is otherwise visible solely as an anonymous-mapping chain in
/// `/proc/<pid>/smaps`. Resolved once per OS process, like [`enabled`].
pub(crate) fn heap_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_GC_TRACE").is_some())
}
/// Writes one per-process-heap line to stderr when [`heap_enabled`]; a no-op otherwise.
///
/// A diagnostic only — no behaviour depends on it, and with the variable unset a call site
/// costs the one cached `OnceLock` load of [`heap_enabled`].
pub(crate) fn heap(args: fmt::Arguments<'_>) {
    if heap_enabled() {
        eprintln!("[rt-heap] {args}");
    }
}

/// Whether the **arena allocation attribution** census is on (`BEAM_RT_ARENA_TRACE` present).
///
/// A fifth channel, and unlike the other four it is not just a print gate: it is the switch the
/// runtime latches into [`crate::runtime::Runtime::alloc_words`]'s census flag, so with the
/// variable unset the arena's whole attribution cost is one `bool` test per allocation. It answers
/// *which* requests produce the arena words [`crate::runtime::RetentionStats`] reports — the size
/// histogram, the largest requests, and the nested-run window split
/// ([`crate::runtime::arena_census`]). Resolved once per OS process, like [`enabled`].
pub(crate) fn arena_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_ARENA_TRACE").is_some())
}

/// Writes one arena-census line to stderr when [`arena_enabled`]; a no-op otherwise.
///
/// A diagnostic only — no behaviour depends on it.
pub(crate) fn arena(args: fmt::Arguments<'_>) {
    if arena_enabled() {
        eprintln!("[rt-arena] {args}");
    }
}
