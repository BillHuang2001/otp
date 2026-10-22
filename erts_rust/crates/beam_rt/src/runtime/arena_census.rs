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

//! **Arena allocation attribution** — an env-gated census of *which* requests produce the words
//! [`Runtime::alloc_words`] hands out of the engine's pointer-stable arena.
//!
//! The retention snapshot ([`crate::runtime::RetentionStats`]) reports the arena's cumulative
//! demand as one number, which is enough to show a long in-process `compile:file/2` reaching tens
//! of GB on the table-less `-eval` route and nothing at all about what produced it. A fix for that
//! depends on the answer: a few huge requests point at an off-heap/copy-pattern fix, millions of
//! small cells at a collection. This module measures the difference.
//!
//! Set `BEAM_RT_ARENA_TRACE` to switch it on ([`crate::debug_trace::arena_enabled`]); the runtime
//! latches the answer into a `bool` field when it is built, so with the variable unset
//! [`Runtime::alloc_words`] pays one branch and nothing else, and
//! [`Runtime::arena_census_report`] is `None`.
//!
//! # What it records
//!
//! 1. a **power-of-two size histogram** — every request is bucketed by
//!    `words.next_power_of_two()`, one bucket per bit of a `usize`, counting calls and summing the
//!    words asked for;
//! 2. the **ten largest single requests**, each with a backtrace (`Backtrace::force_capture`,
//!    symbolised lazily at print time) — an `opt-level=3` build inlines heavily, so a frame names
//!    the enclosing function rather than the exact call site;
//! 3. a **non-overlapping inside/outside word split** — each word is attributed to the innermost
//!    open *nested-run window*, or to "no window open";
//! 4. a **per-window word-delta histogram**, so thousands of small nested runs are
//!    distinguishable from one huge one.
//!
//! A *window* is one nested run: `RuntimeContext`'s blocking entries open one
//! ([`Window::enter`]) around the whole run, so every fun-taking BIF arm and every `apply`
//! that re-enters the engine from below is a window. Windows nest (a nested run may apply a fun
//! itself), so the same word is inside several windows; the inside/outside split is attributed to
//! the **innermost** one and therefore stays non-overlapping, while the per-window histogram
//! counts every window's delta separately.
//!
//! # This is instrumentation, not reclamation
//!
//! Nothing here frees, shrinks, caps, resizes or reorders an allocation: the census only observes.
use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::RefCell;

/// How many size buckets the histogram has: one per bit of a `usize` word count.
const BUCKETS: usize = usize::BITS as usize;
/// How many of the largest single requests are retained.
const TOP_REQUESTS: usize = 10;
/// How many backtrace **frames** are rendered per retained request, so one capture cannot flood
/// the report with the runtime's whole call chain.
const TOP_FRAMES: usize = 12;

/// One histogram cell: how many requests landed in this bucket and how many words they asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SizeBucket {
    /// The number of requests (or windows) in this bucket.
    calls: u64,
    /// The summed words those requests asked for (or the summed per-window deltas).
    words: u64,
}

/// One of the [`TOP_REQUESTS`] largest single requests, with the stack it was made from.
#[derive(Debug)]
struct LargestRequest {
    /// The request size, in words.
    words: usize,
    /// The stack captured **at the request**, rendered only when the report is printed.
    backtrace: Backtrace,
}

impl Default for ArenaCensus {
    /// Every bucket empty. A manual impl because `Default` is not derived for an array longer than
    /// 32 elements, and the histogram is one bucket per bit of a `usize`.
    fn default() -> ArenaCensus {
        ArenaCensus {
            sizes: [SizeBucket::default(); BUCKETS],
            largest: Vec::new(),
            inside_words: 0,
            outside_words: 0,
            window_sizes: [SizeBucket::default(); BUCKETS],
            windows_opened: 0,
            outermost_windows: 0,
        }
    }
}

/// The attribution census itself — a plain value so its folding is unit-testable without a runtime
/// or a thread-local in the way.
#[derive(Debug)]
struct ArenaCensus {
    /// Request size histogram, bucketed by `words.next_power_of_two()` (`sizes[k]` holds requests
    /// whose rounded-up size is `2^k`).
    sizes: [SizeBucket; BUCKETS],
    /// The largest single requests seen, sorted largest-first.
    largest: Vec<LargestRequest>,
    /// Words allocated while at least one nested-run window was open (attributed to the
    /// innermost window, so this figure never double counts).
    inside_words: u64,
    /// Words allocated with no window open.
    outside_words: u64,
    /// Per-window delta histogram, bucketed exactly like [`ArenaCensus::sizes`].
    window_sizes: [SizeBucket; BUCKETS],
    /// How many windows were ever opened.
    windows_opened: u64,
    /// How many of those were **outermost** (opened with no other window open).
    outermost_windows: u64,
}

impl ArenaCensus {
    /// Records one served request of `words` words, made with (`in_window`) or without an open
    /// nested-run window.
    fn record_alloc(&mut self, words: usize, in_window: bool) {
        let bucket = &mut self.sizes[bucket_of(words)];
        bucket.calls = bucket.calls.saturating_add(1);
        bucket.words = bucket.words.saturating_add(words as u64);
        if in_window {
            self.inside_words = self.inside_words.saturating_add(words as u64);
        } else {
            self.outside_words = self.outside_words.saturating_add(words as u64);
        }
        self.consider_largest(words);
    }

    /// Keeps `words` among the [`TOP_REQUESTS`] largest requests — the backtrace is captured
    /// **only** for a request that actually qualifies, so the capture's cost is paid at most
    /// `TOP_REQUESTS` times per run (plus once per later improvement of the smallest entry).
    fn consider_largest(&mut self, words: usize) {
        let qualifies = self.largest.len() < TOP_REQUESTS
            || self
                .largest
                .last()
                .is_some_and(|smallest| words > smallest.words);
        if !qualifies {
            return;
        }
        let backtrace = Backtrace::force_capture();
        // Largest-first, so `last()` is always the smallest retained request.
        let at = self
            .largest
            .iter()
            .position(|request| request.words < words)
            .unwrap_or(self.largest.len());
        self.largest.insert(
            at,
            LargestRequest {
                words,
                backtrace,
            },
        );
        self.largest.truncate(TOP_REQUESTS);
    }

    /// Records that a window closed after having allocated `delta` words.
    fn record_window_delta(&mut self, delta: u64) {
        let bucket = &mut self.window_sizes[bucket_of_usize(delta)];
        bucket.calls = bucket.calls.saturating_add(1);
        bucket.words = bucket.words.saturating_add(delta);
    }
}

/// The census of one thread, plus the nested-run window machinery that attributes to it.
///
/// A thread-local rather than a `Runtime` field: the window guards are opened from
/// `RuntimeContext` while a `&mut Runtime` is already borrowed for the run, and the whole-request
/// path ([`record_alloc`]) would otherwise have to be threaded through it. `beam_rt` runs a
/// runtime on one thread (`src/main.rs`'s driver thread), so the thread is the run.
#[derive(Debug, Default)]
struct State {
    /// How many windows are open right now.
    depth: usize,
    /// The [`ArenaCensus::inside_words`] figure as it stood at each open window's entry,
    /// innermost last — a window's delta is the difference at its exit.
    starts: Vec<u64>,
    /// The census the thread's allocations are folded into.
    census: ArenaCensus,
}

impl State {
    /// Opens a window: remembers the inside-word mark to diff at [`State::close`].
    fn open(&mut self) {
        self.starts.push(self.census.inside_words);
        self.depth = self.starts.len();
        self.census.windows_opened = self.census.windows_opened.saturating_add(1);
        if self.depth == 1 {
            self.census.outermost_windows = self.census.outermost_windows.saturating_add(1);
        }
    }

    /// Closes the innermost window, folding its word delta into the per-window histogram.
    ///
    /// A `close` with no open window records nothing: every `open` is paired by [`Window`]'s
    /// `Drop`, so an unpaired close is a broken pairing rather than a run-time condition, and the
    /// mismatch stays visible in the report as `windows_opened` disagreeing with the histogram's
    /// own total instead of being silently absorbed.
    fn close(&mut self) {
        let Some(start) = self.starts.pop() else {
            return;
        };
        self.depth = self.starts.len();
        let delta = self.census.inside_words.saturating_sub(start);
        self.census.record_window_delta(delta);
    }

    /// Records one served request of `words` words, attributing it to the innermost open window.
    fn record_alloc(&mut self, words: usize) {
        let in_window = self.depth > 0;
        self.census.record_alloc(words, in_window);
    }

    /// The report, with the live window depth filled in.
    fn report(&self) -> Vec<String> {
        self.census.report(self.depth)
    }
}

thread_local! {
    /// The **current thread's** census. Lazy, so a thread that never allocates from an arena (an
    /// OS-child port's driver thread, the timer thread) never builds one.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// An open nested-run window: every word allocated from the arena while it is open is attributed
/// to it (or to a window opened inside it), and its own delta is folded into the per-window
/// histogram when it drops.
///
/// The guard is only ever created when `BEAM_RT_ARENA_TRACE` is set
/// ([`Window::enter`] returns `None` otherwise), and it holds no state of its own: the depth and
/// the entry marks live in the thread-local, so a guard neither borrows the runtime nor has to be
/// dropped on the same line it was created.
#[must_use = "the window is open only while the guard is alive"]
pub(crate) struct Window {
    /// The guard is a token; the census state is the thread-local.
    _token: (),
}

impl Window {
    /// Opens a nested-run window when the arena census is on, or `None` when it is off.
    ///
    /// Takes the runtime only to read its latched census flag, so the caller's spelling is
    /// `Window::enter(this.runtime)` at the top of a blocking run.
    pub(crate) fn enter(runtime: &crate::runtime::Runtime) -> Option<Window> {
        if !runtime.arena_census_enabled() {
            return None;
        }
        STATE.with(|state| state.borrow_mut().open());
        Some(Window { _token: () })
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        STATE.with(|state| state.borrow_mut().close());
    }
}

/// Records one **served** arena request of `words` words against the current thread's census.
///
/// Called from `Runtime::alloc_words` only after the arena handed the words out (a refused request
/// handed out nothing) and only while the runtime's census latch is set.
pub(crate) fn record_alloc(words: usize) {
    STATE.with(|state| state.borrow_mut().record_alloc(words));
}

/// The census report, one line per figure — the body of the `[rt-arena]` diagnostic
/// `crate::cli::run` prints when `BEAM_RT_ARENA_TRACE` is set.
pub(crate) fn report() -> Vec<String> {
    STATE.with(|state| state.borrow().report())
}

/// The bucket a request of `words` words lands in: `log2(words.next_power_of_two())`, so bucket
/// `k` holds every request whose rounded-up size is `2^k` (`0` and `1` share bucket `0`).
fn bucket_of(words: usize) -> usize {
    // `next_power_of_two` panics above `1 << (BITS - 1)`, which no arena request can approach but
    // which must not turn a diagnostic into an abort.
    if words > (1usize << (BUCKETS - 2)) {
        return BUCKETS - 1;
    }
    words.next_power_of_two().trailing_zeros() as usize
}

/// [`bucket_of`] for an already-summed word count.
fn bucket_of_usize(words: u64) -> usize {
    usize::try_from(words).map_or(BUCKETS - 1, bucket_of)
}

/// The word range a bucket covers, rendered for the report (`k == 0` is `0..=1`).
fn range_label(bits: usize) -> String {
    let high = 1u128 << bits;
    let low = if bits == 0 { 0 } else { (1u128 << (bits - 1)) + 1 };
    format!("{low}..={high}")
}

/// Renders a captured backtrace's frames, dropping the census's own machinery (the frames between
/// the capture and the arena's real caller say nothing about *who* asked for the words) and
/// stopping after [`TOP_FRAMES`] frames.
fn render_frames(backtrace: &Backtrace) -> Vec<String> {
    split_frames(&backtrace.to_string())
        .into_iter()
        .filter(|frame| !is_census_frame(frame))
        .take(TOP_FRAMES)
        .flatten()
        .collect()
}

/// Splits a `Backtrace`'s rendering into frames: `Backtrace` writes one header line per frame
/// (`  12: symbol`) followed by that frame's `at file:line` lines.
fn split_frames(rendered: &str) -> Vec<Vec<String>> {
    let mut frames: Vec<Vec<String>> = Vec::new();
    for line in rendered.lines() {
        let starts_a_frame = line
            .trim_start()
            .split_once(':')
            .is_some_and(|(number, _)| {
                !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
            });
        if starts_a_frame || frames.is_empty() {
            frames.push(vec![line.to_string()]);
        } else if let Some(current) = frames.last_mut() {
            current.push(line.to_string());
        }
    }
    frames
}

/// Whether a frame belongs to this module — the capture's own machinery, which the report drops.
fn is_census_frame(frame: &[String]) -> bool {
    frame
        .iter()
        .any(|line| line.contains("runtime/arena_census.rs"))
}

/// `part` as a percentage of `whole`, `0.0` when `whole` is `0`.
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64) * 100.0 / (whole as f64)
    }
}

impl ArenaCensus {
    /// The report: the size histogram, the largest requests, the inside/outside split and the
    /// per-window delta histogram, one line per figure. `open_now` is the live window depth, which
    /// the value itself does not know ([`State`] holds it).
    fn report(&self, open_now: usize) -> Vec<String> {
        let total_calls: u64 = self.sizes.iter().map(|bucket| bucket.calls).sum();
        let total_words: u64 = self.sizes.iter().map(|bucket| bucket.words).sum();
        let mut lines = Vec::new();
        lines.push(format!(
            "arena census: calls={total_calls} words={total_words} mean={:.1} words/call",
            if total_calls == 0 {
                0.0
            } else {
                (total_words as f64) / (total_calls as f64)
            }
        ));
        lines.push("size histogram (bucket = the request rounded up to a power of two):".to_string());
        lines.push(format!(
            "  {:>26} {:>14} {:>16} {:>9}",
            "words", "calls", "words", "% of words"
        ));
        for (bits, bucket) in self.sizes.iter().enumerate() {
            if bucket.calls == 0 {
                continue;
            }
            lines.push(format!(
                "  {:>26} {:>14} {:>16} {:>8.2}%",
                range_label(bits),
                bucket.calls,
                bucket.words,
                percent(bucket.words, total_words),
            ));
        }
        let inside_and_outside = self.inside_words.saturating_add(self.outside_words);
        lines.push(format!(
            "nested-run windows: opened={} outermost={} open_now={} \
             inside_words={} ({:.2}%) outside_words={} ({:.2}%) split_adds_up={}",
            self.windows_opened,
            self.outermost_windows,
            open_now,
            self.inside_words,
            percent(self.inside_words, total_words),
            self.outside_words,
            percent(self.outside_words, total_words),
            inside_and_outside == total_words,
        ));
        let window_total: u64 = self.window_sizes.iter().map(|bucket| bucket.calls).sum();
        let window_words: u64 = self.window_sizes.iter().map(|bucket| bucket.words).sum();
        lines.push(format!(
            "per-window delta histogram: windows={window_total} summed_delta_words={window_words} \
             (a nested window's words are also counted by the windows enclosing it)"
        ));
        lines.push(format!(
            "  {:>26} {:>14} {:>16} {:>9}",
            "delta words", "windows", "words", "% of deltas"
        ));
        for (bits, bucket) in self.window_sizes.iter().enumerate() {
            if bucket.calls == 0 {
                continue;
            }
            lines.push(format!(
                "  {:>26} {:>14} {:>16} {:>8.2}%",
                range_label(bits),
                bucket.calls,
                bucket.words,
                percent(bucket.words, window_words),
            ));
        }
        lines.push(format!(
            "top {} largest single requests (frames from the request's own capture; an \
             opt-level=3 build inlines, so a frame names the enclosing function):",
            self.largest.len()
        ));
        for (index, request) in self.largest.iter().enumerate() {
            lines.push(format!("  #{} words={}", index + 1, request.words));
            match request.backtrace.status() {
                BacktraceStatus::Captured => {
                    for frame in render_frames(&request.backtrace) {
                        lines.push(format!("    {frame}"));
                    }
                }
                status => lines.push(format!("    (no backtrace: {status:?})")),
            }
        }
        lines
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_bucketed_by_its_power_of_two_round_up() {
        // `0` and `1` both round up to `1`.
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(1), 0);
        // Every size in `(2^(k-1), 2^k]` lands in bucket `k`.
        assert_eq!(bucket_of(2), 1);
        assert_eq!(bucket_of(3), 2);
        assert_eq!(bucket_of(4), 2);
        assert_eq!(bucket_of(5), 3);
        assert_eq!(bucket_of(1023), 10);
        assert_eq!(bucket_of(1024), 10);
        assert_eq!(bucket_of(1025), 11);
        assert_eq!(bucket_of(1 << 20), 20);
        // The clamp: a request at or above `2^(BITS-1)` saturates into the top bucket instead of
        // panicking inside `next_power_of_two`.
        assert_eq!(bucket_of(1usize << (BUCKETS - 1)), BUCKETS - 1);
        assert_eq!(bucket_of(usize::MAX), BUCKETS - 1);
        assert_eq!(bucket_of_usize(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_of_usize(5), 3);
    }

    #[test]
    fn a_bucket_covers_exactly_its_documented_range() {
        assert_eq!(range_label(0), "0..=1");
        assert_eq!(range_label(1), "2..=2");
        assert_eq!(range_label(2), "3..=4");
        assert_eq!(range_label(10), "513..=1024");
        // The top label is the `usize` width's bucket and must not overflow an `u128` shift.
        assert_eq!(
            range_label(BUCKETS - 1),
            "4611686018427387905..=9223372036854775808"
        );
    }

    #[test]
    fn the_histogram_counts_calls_and_words_per_bucket() {
        let mut census = ArenaCensus::default();
        census.record_alloc(3, false); // bucket 2
        census.record_alloc(4, false); // bucket 2
        census.record_alloc(5, true); // bucket 3
        census.record_alloc(1, true); // bucket 0
        census.record_alloc(0, false); // bucket 0
        assert_eq!(census.sizes[0], SizeBucket { calls: 2, words: 1 });
        assert_eq!(census.sizes[2], SizeBucket { calls: 2, words: 7 });
        assert_eq!(census.sizes[3], SizeBucket { calls: 1, words: 5 });
        let calls: u64 = census.sizes.iter().map(|bucket| bucket.calls).sum();
        let words: u64 = census.sizes.iter().map(|bucket| bucket.words).sum();
        assert_eq!(calls, 5);
        assert_eq!(words, 13);
        // The inside/outside split is non-overlapping and covers exactly the recorded words.
        assert_eq!(census.inside_words, 6);
        assert_eq!(census.outside_words, 7);
        assert_eq!(census.inside_words + census.outside_words, words);
        // The report states the split's consistency rather than a reader having to check it.
        let report = census.report(0).join("\n");
        assert!(report.contains("calls=5 words=13"), "{report}");
        assert!(report.contains("split_adds_up=true"), "{report}");
        assert!(report.contains("3..=4"), "{report}");
    }

    #[test]
    fn only_the_ten_largest_requests_are_retained_largest_first() {
        let mut census = ArenaCensus::default();
        for words in 1..=50u64 {
            census.record_alloc(words as usize, false);
        }
        assert_eq!(census.largest.len(), TOP_REQUESTS);
        let sizes: Vec<usize> = census.largest.iter().map(|request| request.words).collect();
        assert_eq!(sizes, (41..=50).rev().collect::<Vec<usize>>());
        // A request smaller than every retained one changes nothing.
        census.record_alloc(1, false);
        assert_eq!(census.largest[0].words, 50);
        assert_eq!(census.largest.len(), TOP_REQUESTS);
    }

    #[test]
    fn a_window_folds_its_own_delta_and_nests_without_double_counting() {
        let mut state = State::default();
        // Outer window: 100 words with nothing else open.
        state.open();
        state.record_alloc(100);
        // Inner window: 4 words while the outer one is still open.
        state.open();
        state.record_alloc(4);
        state.close();
        // The outer window's own remaining 3 words.
        state.record_alloc(3);
        state.close();
        assert_eq!(state.census.windows_opened, 2);
        assert_eq!(state.census.outermost_windows, 1);
        assert_eq!(state.depth, 0);
        // The 107 words are *inside* a window; nothing was allocated outside one.
        assert_eq!(state.census.inside_words, 107);
        assert_eq!(state.census.outside_words, 0);
        // Each window recorded its own delta: 4 for the inner one, 107 for the outer.
        assert_eq!(state.census.window_sizes[bucket_of(4)].calls, 1);
        assert_eq!(state.census.window_sizes[bucket_of(4)].words, 4);
        assert_eq!(state.census.window_sizes[bucket_of(107)].calls, 1);
        assert_eq!(state.census.window_sizes[bucket_of(107)].words, 107);
        let windows: u64 = state.census.window_sizes.iter().map(|b| b.calls).sum();
        assert_eq!(windows, 2);
    }

    #[test]
    fn words_outside_any_window_are_attributed_outside() {
        let mut state = State::default();
        state.record_alloc(8);
        state.open();
        state.record_alloc(2);
        state.close();
        assert_eq!(state.census.outside_words, 8);
        assert_eq!(state.census.inside_words, 2);
        assert_eq!(state.census.outermost_windows, 1);
    }

    #[test]
    fn an_unpaired_close_records_nothing_and_does_not_underflow() {
        let mut state = State::default();
        state.close();
        assert_eq!(state.depth, 0);
        assert_eq!(state.census.windows_opened, 0);
        let windows: u64 = state.census.window_sizes.iter().map(|b| b.calls).sum();
        assert_eq!(windows, 0);
    }

    #[test]
    fn a_rendered_backtrace_splits_into_frames_and_the_census_frames_are_dropped() {
        let rendered = "   0: std::sys::backtrace::__rust_begin_short_backtrace\n\
                        \x20            at /rustc/x/library/std/src/sys/backtrace.rs:1:2\n\
                        \x20  1: beam_rt::runtime::arena_census::record_alloc\n\
                        \x20            at ./erts_rust/crates/beam_rt/src/runtime/arena_census.rs:277:43\n\
                        \x20  2: <beam_rt::runtime::Runtime>::alloc_words\n\
                        \x20            at ./erts_rust/crates/beam_rt/src/runtime.rs:2352:9\n";
        let frames = split_frames(rendered);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1].len(), 2);
        assert!(!is_census_frame(&frames[0]));
        assert!(is_census_frame(&frames[1]));
        assert!(!is_census_frame(&frames[2]));
        // The census's own frames are the ones between the capture and the real caller.
        let rendered_kept: Vec<String> = frames
            .into_iter()
            .filter(|frame| !is_census_frame(frame))
            .flatten()
            .collect();
        assert!(rendered_kept[0].contains("__rust_begin_short_backtrace"));
        assert!(rendered_kept.iter().any(|line| line.contains("alloc_words")));
        assert!(!rendered_kept.iter().any(|line| line.contains("arena_census.rs")));
    }

    #[test]
    fn an_empty_census_reports_zeros_without_dividing_by_zero() {
        let report = ArenaCensus::default().report(0).join("\n");
        assert!(report.contains("calls=0 words=0 mean=0.0 words/call"), "{report}");
        assert!(report.contains("nested-run windows: opened=0"), "{report}");
        assert!(report.contains("split_adds_up=true"), "{report}");
        assert!(report.contains("top 0 largest single requests"), "{report}");
    }
}
