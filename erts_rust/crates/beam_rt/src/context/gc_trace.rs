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

//! TEMPORARY investigation scaffold for the A.9 selfhost corruption.
//!
//! Env-gated and inert unless `BEAM_RT_TRACE_GC` (logging) or `BEAM_RT_FORCE_DESCEND`
//! (descent-gate override) is set. Not to be committed.

#![allow(unsafe_code)]

use std::sync::OnceLock;

use beam_process::Process;
use beam_term::Eterm;

/// Whether the logging scaffold is active (`BEAM_RT_TRACE_GC`).
pub(crate) fn enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        let on = std::env::var_os("BEAM_RT_TRACE_GC").is_some();
        if on {
            // Keep panics visible but tagged, so the invalid-term probes stay readable.
            std::panic::set_hook(Box::new(|info| {
                eprintln!("GCT PANIC {}", info.payload());
            }));
        }
        on
    })
}

/// Whether the descent gate must be treated as open (`BEAM_RT_FORCE_DESCEND`).
pub(crate) fn force_descend() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("BEAM_RT_FORCE_DESCEND").is_some())
}

/// Emits one trace line (no-op unless enabled).
#[inline]
pub(crate) fn log(args: std::fmt::Arguments<'_>) {
    if enabled() {
        eprintln!("GCT {args}");
    }
}

/// The process's heap geometry: the bump block, the current nursery space, the space count and
/// the mature space — the ranges a relocation moves terms between.
pub(crate) fn geom(process: &Process) -> String {
    let collector = process.collector();
    let young = collector.young();
    let heap = process.heap();
    format!(
        "heap=[0x{:x},0x{:x}) used={} young=[0x{:x},0x{:x}) ylim=0x{:x} yused={} ycap={} spaces={} mature=[0x{:x},+{})",
        heap.hp() as usize,
        heap.hend() as usize,
        heap.used(),
        young.base() as usize,
        young.top() as usize,
        young.limit() as usize,
        young.used_words(),
        young.capacity_words(),
        collector.young_space_count(),
        collector.mature_base() as usize,
        collector.mature_capacity_words(),
    )
}

/// Whether a term can still be decoded — the non-panicking form of
/// [`beam_term::ops::class`], used to spot a word whose header was overwritten.
pub(crate) fn sane(term: Eterm) -> bool {
    if !beam_term::prelude::is_boxed(term) && !beam_term::prelude::is_list(term) {
        return true;
    }
    // SAFETY: nothing is written; the classifier only reads the header word of a term the
    // caller still holds, and a term that is not boxed/cons never reaches here.
    std::panic::catch_unwind(|| unsafe { beam_term::ops::class(term) }).is_ok()
}

/// Where a term currently lives, relative to the collector's scanned regions.
pub(crate) fn region(process: &Process, term: Eterm) -> &'static str {
    if !beam_term::prelude::is_boxed(term) && !beam_term::prelude::is_list(term) {
        return "immediate";
    }
    if beam_term::literal::is_literal(term) {
        return "literal";
    }
    let ptr = beam_term::list::ptr_val(term) as *const Eterm;
    if process.collector().young_contains(ptr) {
        "young"
    } else if process.collector().mature_contains(ptr) {
        "mature"
    } else {
        "OTHER"
    }
}

/// A compact dump of a root list: `index:0xaddr=region`, with `!` marking a word that no longer
/// decodes as a term (only boxed/cons words are listed).
pub(crate) fn roots_dump<'a, I: IntoIterator<Item = &'a Eterm>>(
    process: &Process,
    words: I,
) -> String {
    let mut out = String::new();
    for (index, &word) in words.into_iter().enumerate() {
        if index >= 48 {
            out.push_str(" ...");
            break;
        }
        if !beam_term::prelude::is_boxed(word) && !beam_term::prelude::is_list(word) {
            continue;
        }
        out.push_str(&format!(
            " {index}:0x{:x}={}{}",
            word.raw(),
            region(process, word),
            if sane(word) { "" } else { "!BAD" }
        ));
    }
    out
}