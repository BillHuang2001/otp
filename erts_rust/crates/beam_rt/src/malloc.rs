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

//! The **libc allocator's free-page release** — the crate's marked `unsafe` module.
//!
//! glibc's `malloc` grows a per-thread *arena* and, when a chunk is freed, keeps the page in
//! the arena's free lists instead of returning it to the kernel. For a short-lived process
//! that is invisible; for this runtime it is the largest single item of *retained* resident
//! memory, because both of its phases have a transient high-water much larger than their
//! steady state:
//!
//! | phase | peak RSS | arena free at exit | live at exit |
//! |---|---|---|---|
//! | bare boot (`-eval 'halt(0).'`) | 115 MB | 77.2 MB | 211 kB |
//! | in-process compile of `cerl_clauses.erl` (13.8 kB) | 314 MB | 201.1 MB | 219 kB |
//!
//! So ~99.9 % of the allocator's *system* bytes are free and unreachable. glibc never
//! decrements that bookkeeping on its own from a `free` of a small chunk, and it exposes
//! exactly one lever to return the pages: [`libc::malloc_trim`], which walks **every** arena
//! (`__malloc_trim`'s arena list) and hands the free ranges back with `madvise`. Measured on
//! this host (glibc 2.44): a 40 MB all-chunks-freed churn drops its RSS from 52 428 kB to
//! 1 880 kB after one `malloc_trim(0)` — while `mallinfo2().arena` does **not** shrink, which
//! is why the trim is verified by RSS and never by that counter.
//!
//! # Why it must be called on the thread that owns the arena
//!
//! An arena belongs to the thread that first allocated on it, and glibc attaches a thread to
//! a fresh arena for its own allocations (this runtime's work — the boot drive and every
//! nested run — happens on the `beam_rt-driver` thread `src/main.rs` spawns, whose arena is
//! `Arena 1` in `malloc_stats()`; the main thread's own arena stays at 135 kB). A trim
//! performed *on another thread's behalf* — e.g. from the main thread at `atexit` — releases
//! the **main** arena and leaves the driver thread's high-water untouched: measured, that
//! call returned 1 byte. Every call site of [`release_free_pages`] is therefore a point on
//! the driver thread, and never on a helper thread.
//!
//! # Cost (it must stay off every hot path)
//!
//! One call is a walk over the arena list plus one `madvise` per free range — O(free ranges),
//! not O(allocation) — and it is made exactly **once** per process, at the one point that is
//! both quiescent and complete: `src/main.rs`'s `run_on_driver_stack`, after the driver thread
//! has been joined and before the process status is returned.
//!
//! Two earlier, "obvious" points were measured and rejected. At the boot→action boundary the
//! arena is 97 681 kB **in use** of 97 948 kB — 266 kB free, so there is nothing to release. At
//! the end of `crate::cli::run` the same holds (266 kB free on a bare boot, 1.8 MB after an
//! in-process compile), and even with the runtime dropped there the thread still holds most of
//! its arena: 88 840 kB resident after the call on a bare boot, versus ~18 MB for the post-join
//! placement. `src/cli.rs` records the same finding next to that point.
//!
//! It is **not** on the scheduler pass, the runner's park arm, a BIF, a receive or any other
//! per-call/per-slice path: nothing in the runtime's execution semantics may pay for it.
//!
//! # Behaviour neutrality
//!
//! The lever touches only the allocator's *free-page* bookkeeping: no term, no heap, no
//! table and no live allocation is read, moved or freed, so no result, exception, exit code
//! or `system_info`/`process_info` answer can change. The released pages are re-faulted
//! transparently if the following work needs them again (a trim at point 1 costs a
//! re-fault of whatever the action re-uses, which is why the calls are placed at quiescent
//! points and not inside a run). On a non-glibc target — musl's `malloc` already releases
//! every free page — the module is an honest no-op: [`release_free_pages`] answers `None`
//! rather than pretending to have a lever.

// The crate is `#![deny(unsafe_code)]` (workspace `CONTEXT.md` §3): `unsafe` is confined to
// explicitly-marked modules, and this is one of them. The module's *entire* raw surface is the
// single `malloc_trim` entry point in [`release_free_pages`], a call into the C allocator that
// takes no pointer, holds no borrow and returns a status word.
#![allow(unsafe_code)]

/// Returns the libc allocator's free pages to the operating system: `Some(released)` where the
/// platform has such a lever (`true` when this call actually returned memory to the kernel,
/// which is glibc's own meaning for `malloc_trim`'s status), `None` where it has none.
///
/// Called at the one quiescent point documented at the module level; **never** on a hot path.
/// `Some(_)` is not a claim that every free page went back — glibc's trim is limited by the
/// live chunks above a free range in each arena — so the honest figure to read is the process's
/// RSS before and after, not this word.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn release_free_pages() -> Option<bool> {
    // The `pad` argument is the free space glibc must leave *in* each arena (its own
    // `M_TRIM_THRESHOLD`-style slack). `0` is "keep nothing", i.e. return as much as the live
    // allocations allow — the same call `MALLOC_TRIM_THRESHOLD_`-style tuning would make, minus
    // the tunable's 128 kB default slack, which is noise against a 200 MB high-water.
    //
    // SAFETY: `malloc_trim` is glibc's own allocator entry point. It takes a `size_t` by value,
    // returns an `int` status, takes every arena lock it touches itself, holds no borrow of Rust
    // memory and frees no live allocation — its only effect is `madvise`/`sbrk` on pages the
    // allocator already considers free. It is therefore sound to call from any thread at any
    // point at which the allocator itself may be called (i.e. not from a signal handler or
    // inside the allocator, neither of which happens here).
    let status = unsafe { libc::malloc_trim(0) };
    let released = status != 0;
    // The `BEAM_RT_MEM_TRACE` channel, silent unless set: one line reporting whether the
    // allocator actually returned anything, which is the figure that makes a placement mistake
    // (a trim while the runtime is still live) visible instead of invisible.
    crate::debug_trace::mem(format_args!("heap-release={released:?}"));
    Some(released)
}

/// `None`: this platform's allocator has no free-page lever this module could pull (musl and
/// the BSD allocators already return freed pages to the OS on `free`).
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn release_free_pages() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::release_free_pages;

    /// The lever must be **wired to the platform allocator**, never silently a no-op on a
    /// platform that has one: this is the assertion that keeps the module honest if the `cfg`
    /// gate and the target ever disagree.
    #[test]
    fn the_platform_lever_is_wired_up() {
        let outcome = release_free_pages();
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        assert!(
            outcome.is_some(),
            "a glibc host must reach malloc_trim, not the no-op arm"
        );
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        assert_eq!(outcome, None, "only glibc has the trim lever");
    }

    /// Both call sites may run back to back (the boot boundary and the exit), and a second trim
    /// of an already-trimmed arena is legal, cheap and reports the same shape of answer.
    #[test]
    fn calling_it_twice_is_harmless() {
        let first = release_free_pages();
        let second = release_free_pages();
        assert_eq!(
            first.is_some(),
            second.is_some(),
            "the two calls must answer the same `Option` shape"
        );
    }

    /// The **effect** — free pages really go back to the kernel — measured as process RSS.
    ///
    /// `#[ignore]`d because it is a whole-process measurement: a concurrently running test in
    /// the same binary allocates too, which can only inflate RSS and mask the release. Run it
    /// alone: `cargo test -p beam_rt --lib -- --ignored --test-threads=1
    /// malloc::tests::free_pages_go_back_to_the_kernel`.
    #[test]
    #[ignore = "whole-process RSS measurement; run with --test-threads=1"]
    fn free_pages_go_back_to_the_kernel() {
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        {
            assert_eq!(release_free_pages(), None);
        }
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            // 4 KiB chunks: the size band the runtime's own transient work allocates in, and
            // far below glibc's mmap threshold, so every byte is arena memory (an mmap'd block
            // would be returned on `free` and prove nothing about the trim).
            const CHUNKS: usize = 24_576;
            const CHUNK: usize = 4096;
            let before = resident_kb();
            let mut pages: Vec<Vec<u8>> = (0..CHUNKS).map(|_| vec![0xa5u8; CHUNK]).collect();
            let filled = resident_kb();
            pages.clear();
            let released = release_free_pages();
            let after = resident_kb();
            assert!(
                filled >= before + (CHUNKS * CHUNK / 1024) / 2,
                "the churn must be visible in RSS: {before} kB -> {filled} kB"
            );
            assert_eq!(released, Some(true), "glibc reports a releasing trim");
            assert!(
                after + (CHUNKS * CHUNK / 1024) / 2 <= filled,
                "at least half of the freed 96 MiB must leave RSS: {filled} kB -> {after} kB"
            );
        }
    }

    /// This process's resident set in KiB, as `/proc/self/status` reports it (already in kB, so
    /// the measurement makes no page-size assumption).
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn resident_kb() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        let line = status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .expect("VmRSS is in /proc/self/status");
        line.split_whitespace()
            .nth(1)
            .and_then(|field| field.parse().ok())
            .expect("VmRSS's value is a number of kB")
    }
}
