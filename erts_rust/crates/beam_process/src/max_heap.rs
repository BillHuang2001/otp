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

//! The per-process `max_heap_size` **policy**: the setting, its flags, the usage accounting and the
//! exceed verdict — the authoritative model of the C's per-process `MAX_HEAP_SIZE(p)` /
//! `MAX_HEAP_SIZE_FLAGS(p)` pair (`erts/emulator/beam/erl_process.h`, checked by
//! `erl_gc.c:check_max_heap_size`).
//!
//! This module is a **pure decision surface**: it owns the types and the arithmetic, and never
//! mutates a process. [`Process`](crate::Process) owns one [`MaxHeapSetting`] per PCB and records
//! the [`HeapLimitVerdict`] an exceed produces (so a host can drain it); acting on the verdict —
//! terminating the process with the bare atom `killed`, or emitting the `=ERROR REPORT====` — is
//! the runtime's, because only the runtime owns the process table.
//!
//! # What the oracle actually does (probed against OTP 29.1.1 / erts-17.1)
//!
//! * `size` is in **words**, stored **raw** (no growth-table rounding), and `size == 0` means *no
//!   limit at all*.
//! * The check is **`used > size` — strict** — and it runs **only at a collection / heap-growth
//!   decision**, never per allocation.
//! * `used` is the **prospective post-collection capacity**, not the heap's occupied words: the
//!   total heap the collection has just decided to *install* (nursery + stack + old heap),
//!   **including** the growth the admission required — plus, with `include_shared_binaries`, the
//!   off-heap binary payload. A port that compared only the *current* occupied size would graze far
//!   below the limit and never fire. Evidence: `#{size => 1000}` fired at
//!   `Total Heap Size: 1299`, `#{size => 50000}` with a 20k-message mailbox fired at 95419.
//! * `kill => true` (the default) raises **no exception**: the process **self-exits with the bare
//!   atom `killed`** (not `kill`), untrappable by the victim, a trapping linked peer observes
//!   `{'EXIT', Pid, killed}` and a monitoring peer a `'DOWN' … killed`. See [`HeapLimitVerdict::Kill`].
//! * `kill => false` raises **nothing** and kills **nothing**: the process survives above the limit
//!   (logging, with `error_logger => true`, an `=ERROR REPORT====` with `Context: maximum heap size
//!   reached` on every subsequent collection while over). See [`HeapLimitVerdict::Log`].
//! * `include_shared_binaries` (default **false**) adds the off-heap refc-binary payload to the
//!   charge: the same probe survives with `false` and is killed with `true`.
//! * The flags are independent: `error_logger` governs the report on its own, so
//!   `kill: false, error_logger: true` logs at **every** over-limit collection.
//!
//! # There is deliberately no raise path
//!
//! `kill => false` is **log-only** empirically, so there is no third "keep running *and* raise"
//! outcome and [`HeapLimitVerdict`] has no `Raise` variant: fabricating one (`system_limit` or any
//! other) would contradict the oracle. A `max_heap_size` breach is never an exception — it is a
//! termination ([`HeapLimitVerdict::Kill`]) or a log line ([`HeapLimitVerdict::Log`]).
//!
//! This is **not** the same mechanism as [`HeapPolicy::max_heap_words`](beam_gc::HeapPolicy):
//! that one is a *growth clamp* which makes the allocator fail with the typed heap
//! `out_of_memory` (rendered `error:system_limit`), carries no flags and logs nothing. Both can be
//! set on one process and they are independent.
//!
//! # The `process_info(P, garbage_collection)` rendering
//!
//! A setting renders as the four-key map `#{error_logger => B, include_shared_binaries => B,
//! kill => B, size => N}`, in that (the oracle's) key order. That rendering is `beam_bifs`'/
//! `beam_rt`'s job (it builds terms); this module only carries the values.

use core::mem::size_of;

use beam_term::prelude::Eterm;

/// The `max_heap_size` boolean flags — the C `H_MAX_FLAGS` word
/// (`erts/emulator/beam/erl_init.c:814-815`).
///
/// They select the **exceed behaviour**; the bound itself is
/// [`MaxHeapSetting::size`]. The defaults are the C's
/// [`Default`] impl: `kill` and `error_logger` on, shared binaries excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxHeapFlags {
    /// `kill` — on a breach the process **self-exits with the bare atom `killed`**
    /// (untrappable by the victim). When `false` the process keeps running.
    pub kill: bool,
    /// `error_logger` — on a breach emit an `=ERROR REPORT====` with
    /// `Context: maximum heap size reached`. Independent of [`Self::kill`]: `kill: false,
    /// error_logger: true` logs on **every** subsequent over-limit collection.
    pub error_logger: bool,
    /// `include_shared_binaries` — count the off-heap refc-binary payload towards the bound (the
    /// C's `MAX_HEAP_SIZE_INCLUDE_OH_BINS`). Off by default: the same workload survives with it
    /// `false` and is killed with it `true`.
    pub include_shared_binaries: bool,
}

impl Default for MaxHeapFlags {
    /// The emulator defaults (`H_MAX_FLAGS`, `erl_init.c:814-815`): kill on, error logger on,
    /// shared binaries excluded.
    fn default() -> Self {
        MaxHeapFlags {
            kill: true,
            error_logger: true,
            include_shared_binaries: false,
        }
    }
}

/// One process's `max_heap_size` **setting**: the word bound plus its flags.
///
/// The value the C keeps per process (`MAX_HEAP_SIZE_GET(p)` / `MAX_HEAP_SIZE_FLAGS_GET(p)`) and
/// what `erlang:process_flag(max_heap_size, …)` replaces wholesale. A process starts
/// [`unlimited`](MaxHeapSetting::unlimited) with [`MaxHeapFlags::default`].
///
/// `size == 0` is the C's *no upper bound* ([`is_unlimited`](MaxHeapSetting::is_unlimited)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxHeapSetting {
    /// The bound in **words**, stored **raw** (no growth-table rounding); `0` = unlimited.
    pub size: usize,
    /// The `kill`/`error_logger`/`include_shared_binaries` flags.
    pub flags: MaxHeapFlags,
}

impl MaxHeapSetting {
    /// The setting a process with **no explicit bound** gets: `size == 0` with `flags`.
    pub fn unlimited(flags: MaxHeapFlags) -> MaxHeapSetting {
        MaxHeapSetting { size: 0, flags }
    }

    /// Whether this setting imposes **no** bound (`size == 0`).
    pub fn is_unlimited(&self) -> bool {
        self.size == 0
    }

    /// Whether this setting's bound is **below the floor** `min_heap_words`, so the parse site must
    /// reject it with `badarg` — the oracle's rule `size != 0 && size < min_heap_size`.
    ///
    /// Single-sourced here because the two call sites use **different** floors: `spawn_opt` the
    /// rounded `min_heap_size` its own options computed (`erl_process.c:12429`), and
    /// `erlang:process_flag/2` the **calling** process's `MIN_HEAP_SIZE(p)` (`bif.c:2155`). A
    /// `min_heap_size` set *after* a `max_heap_size` is deliberately **not** re-validated — the
    /// oracle does not reject that direction either.
    pub fn violates_floor(&self, min_heap_words: usize) -> bool {
        self.size != 0 && self.size < min_heap_words
    }

    /// The verdict for `usage` — the C's `check_max_heap_size` decision.
    ///
    /// [`Within`](HeapLimitVerdict::Within) when the setting is unlimited or the charge does not
    /// exceed the bound; otherwise [`Kill`](HeapLimitVerdict::Kill) when
    /// [`MaxHeapFlags::kill`] is set, else [`Log`](HeapLimitVerdict::Log).
    ///
    /// The comparison is **strict**: a charge exactly equal to `size` is
    /// [`Within`](HeapLimitVerdict::Within), one word more is not.
    pub fn verdict(&self, usage: MaxHeapUsage) -> HeapLimitVerdict {
        if self.is_unlimited() {
            return HeapLimitVerdict::Within;
        }
        if usage.charged_words(self.flags) > self.size {
            if self.flags.kill {
                HeapLimitVerdict::Kill
            } else {
                HeapLimitVerdict::Log
            }
        } else {
            HeapLimitVerdict::Within
        }
    }
}

/// What a process would be **charged** against its `max_heap_size` at a collection decision.
///
/// Both halves are capacities/payloads, not occupied words:
///
/// * `heap_words` — the **prospective total heap capacity** the collection installs
///   ([`AnyCollector::total_heap_size_words`](beam_gc::AnyCollector) in this crate's terms: the
///   nursery chain's capacity plus the mature generation's, i.e. the `Total Heap Size` the oracle
///   prints in its report);
/// * `off_heap_binary_bytes` — the **payload bytes** of the off-heap refc binaries the process
///   owns, counted only when [`MaxHeapFlags::include_shared_binaries`] is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxHeapUsage {
    /// The total heap capacity in words (nursery + mature), not the occupied words.
    pub heap_words: usize,
    /// The off-heap refc-binary payload in bytes.
    pub off_heap_binary_bytes: usize,
}

impl MaxHeapUsage {
    /// The usage `heap_words` of heap capacity with `off_heap_binary_bytes` of off-heap payload.
    pub fn new(heap_words: usize, off_heap_binary_bytes: usize) -> MaxHeapUsage {
        MaxHeapUsage {
            heap_words,
            off_heap_binary_bytes,
        }
    }

    /// The word charge the flags select: the heap capacity alone, plus — with
    /// [`MaxHeapFlags::include_shared_binaries`] — the off-heap payload **rounded up to whole
    /// words** (`ceil(bytes / size_of::<Eterm>())`).
    ///
    /// The rounding is the C's word arithmetic, which only ever counts whole words. Honest gap: the
    /// C also adds the process's `bin_old_vheap` (its binary virtual-heap bookkeeping) to the
    /// charge; this crate does not model a binary vheap, so that term is absent — which makes the
    /// charge here a small **under**-estimate against the oracle, never an over-estimate.
    pub fn charged_words(&self, flags: MaxHeapFlags) -> usize {
        let binaries = if flags.include_shared_binaries {
            // `size_of::<Eterm>()` is a non-zero power of two, so this cannot divide by zero.
            self.off_heap_binary_bytes.div_ceil(size_of::<Eterm>())
        } else {
            0
        };
        self.heap_words.saturating_add(binaries)
    }
}

/// The outcome of a `max_heap_size` check — what the runtime must **do** about it.
///
/// Produced by [`MaxHeapSetting::verdict`] and
/// [`Process::heap_limit_verdict`](crate::Process::heap_limit_verdict); recorded on the PCB by the
/// collection/growth doors and drained by the host. There is deliberately **no** `Raise` variant:
/// the oracle has no exception path for this limit (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeapLimitVerdict {
    /// The charge is within the bound (or the setting is unlimited): nothing to do.
    Within,
    /// The charge exceeds the bound **and** [`MaxHeapFlags::kill`] is set: the host must
    /// **self-exit the process with the bare atom `killed`** — not `kill`, and not an exception.
    /// The exit is untrappable by the victim; a trapping linked peer sees
    /// `{'EXIT', Pid, killed}` and survives, and a monitoring peer a `'DOWN' … killed`.
    Kill,
    /// The charge exceeds the bound and `kill` is **not** set: the host emits an
    /// `=ERROR REPORT====` with `Context: maximum heap size reached` (plus `Max Heap Size`,
    /// `Total Heap Size`, `Kill: false`, `Error Logger:`, `Message Queue Len`, `GC Info` and a
    /// `Stacktrace`) when [`MaxHeapFlags::error_logger`] is set, and **the process keeps
    /// running**. Nothing is killed and nothing is raised.
    Log,
}

impl HeapLimitVerdict {
    /// Whether this verdict is a breach (anything but [`Within`](HeapLimitVerdict::Within)).
    pub fn is_breach(self) -> bool {
        !matches!(self, HeapLimitVerdict::Within)
    }
}

#[cfg(test)]
mod tests {
    //! The pure truth table: the strict `>` comparison, the `size == 0` escape, the
    //! `include_shared_binaries` charge, the flag selection and the floor rule.

    use super::*;

    fn setting(size: usize, flags: MaxHeapFlags) -> MaxHeapSetting {
        MaxHeapSetting { size, flags }
    }

    fn killing() -> MaxHeapFlags {
        MaxHeapFlags {
            kill: true,
            error_logger: true,
            include_shared_binaries: false,
        }
    }

    fn logging_only() -> MaxHeapFlags {
        MaxHeapFlags {
            kill: false,
            error_logger: true,
            include_shared_binaries: false,
        }
    }

    #[test]
    fn default_flags_are_kill_log_and_no_shared_binaries() {
        let flags = MaxHeapFlags::default();
        assert!(flags.kill, "the C `H_MAX_FLAGS` kills by default");
        assert!(flags.error_logger, "and logs");
        assert!(
            !flags.include_shared_binaries,
            "and does not count off-heap binaries"
        );
    }

    #[test]
    fn an_unlimited_setting_is_always_within() {
        let unlimited = MaxHeapSetting::unlimited(MaxHeapFlags::default());
        assert!(unlimited.is_unlimited());
        assert_eq!(unlimited.size, 0);
        for heap_words in [0usize, 1, 233, 1 << 20, usize::MAX - 1] {
            assert_eq!(
                unlimited.verdict(MaxHeapUsage::new(heap_words, usize::MAX)),
                HeapLimitVerdict::Within,
                "size 0 is the C's `no upper bound`, whatever the usage"
            );
        }
    }

    #[test]
    fn the_comparison_is_strict() {
        let bound = setting(1000, killing());
        assert!(!bound.is_unlimited());

        assert_eq!(
            bound.verdict(MaxHeapUsage::new(999, 0)),
            HeapLimitVerdict::Within,
            "below the bound"
        );
        assert_eq!(
            bound.verdict(MaxHeapUsage::new(1000, 0)),
            HeapLimitVerdict::Within,
            "exactly at the bound is still within — the oracle's test is `>`"
        );
        assert_eq!(
            bound.verdict(MaxHeapUsage::new(1001, 0)),
            HeapLimitVerdict::Kill,
            "one word over the bound is not"
        );
    }

    #[test]
    fn the_kill_flag_selects_kill_or_log_and_nothing_raises() {
        let usage = MaxHeapUsage::new(1001, 0);
        assert_eq!(
            setting(1000, killing()).verdict(usage),
            HeapLimitVerdict::Kill
        );
        assert_eq!(
            setting(1000, logging_only()).verdict(usage),
            HeapLimitVerdict::Log,
            "kill: false is log-only; the process survives and nothing is raised"
        );

        // `error_logger` does not change the verdict — it governs the report at the log site.
        let silent_logger = MaxHeapFlags {
            kill: false,
            error_logger: false,
            include_shared_binaries: false,
        };
        assert_eq!(
            setting(1000, silent_logger).verdict(usage),
            HeapLimitVerdict::Log,
            "the verdict is a breach either way; only the host's report is suppressed"
        );
        assert_eq!(
            setting(1000, silent_logger).verdict(MaxHeapUsage::new(10, 0)),
            HeapLimitVerdict::Within
        );
    }

    #[test]
    fn include_shared_binaries_adds_the_payload_rounded_up_to_words() {
        // One word is `size_of::<Eterm>()` bytes.
        let word = size_of::<Eterm>();
        assert_eq!(word, 8, "a term is one machine word on the 64-bit target");

        let payload = 8 * 300 + 1; // 300 words + 1 byte
        let usage = MaxHeapUsage::new(1000, payload);
        assert_eq!(
            usage.charged_words(killing()),
            1000,
            "the flag is off, so the payload is not charged"
        );
        assert_eq!(
            usage.charged_words(MaxHeapFlags {
                include_shared_binaries: true,
                ..killing()
            }),
            1000 + 301,
            "the payload is charged rounded UP to whole words"
        );
        assert_eq!(
            MaxHeapUsage::new(1000, word * 4).charged_words(MaxHeapFlags {
                include_shared_binaries: true,
                ..killing()
            }),
            1004,
            "an exact multiple needs no extra word"
        );
        assert_eq!(
            MaxHeapUsage::new(1000, 0).charged_words(MaxHeapFlags {
                include_shared_binaries: true,
                ..killing()
            }),
            1000,
            "no payload, no charge"
        );

        // A payload-only breach: the heap alone is within, the payload pushes it over.
        let sharing = MaxHeapFlags {
            include_shared_binaries: true,
            ..killing()
        };
        let bound = setting(1000, sharing);
        assert_eq!(
            bound.verdict(MaxHeapUsage::new(1000, 0)),
            HeapLimitVerdict::Within
        );
        assert_eq!(
            bound.verdict(MaxHeapUsage::new(1000, word)),
            HeapLimitVerdict::Kill,
            "one word of payload over the bound fires"
        );
    }

    #[test]
    fn the_charge_saturates_rather_than_overflowing() {
        let usage = MaxHeapUsage::new(usize::MAX, usize::MAX);
        assert_eq!(
            usage.charged_words(MaxHeapFlags {
                include_shared_binaries: true,
                ..killing()
            }),
            usize::MAX,
            "a hostile pair saturates; it never wraps into a within-limit charge"
        );
    }

    #[test]
    fn violates_floor_is_the_oracles_size_below_min_heap_size_rule() {
        assert!(
            !setting(0, killing()).violates_floor(233),
            "size 0 is unlimited, so no floor applies"
        );
        assert!(
            !setting(0, killing()).violates_floor(0),
            "unlimited is never below a zero floor"
        );
        assert!(
            setting(100, killing()).violates_floor(233),
            "100 < 233 violates the floor"
        );
        assert!(
            !setting(233, killing()).violates_floor(233),
            "equal to the floor is accepted"
        );
        assert!(
            !setting(234, killing()).violates_floor(233),
            "above the floor is accepted"
        );
        assert!(
            !setting(1, killing()).violates_floor(0),
            "a zero floor (this crate's default policy) rejects nothing"
        );
    }

    #[test]
    fn a_verdict_knows_whether_it_is_a_breach() {
        assert!(!HeapLimitVerdict::Within.is_breach());
        assert!(HeapLimitVerdict::Kill.is_breach());
        assert!(HeapLimitVerdict::Log.is_breach());
    }
}
