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

//! **The region-reclaim holder diagnostic** (`BEAM_RT_STORE_TRACE`) — naming the *concrete* holder
//! of a stale word, not just its family and slot.
//!
//! Two refusals of [`crate::runtime::reclaim`] say "a live word stands somewhere the pass would
//! drop". Their published counters name the family and slot
//! ([`RegionReclaimStats::store_move_hit`](crate::runtime::RegionReclaimStats::store_move_hit)) or
//! the header word a walk could not classify
//! ([`RegionReclaimStats::reclaim_last_decline_header`](crate::runtime::RegionReclaimStats::reclaim_last_decline_header)),
//! and a fix needs more than that: *which* dictionary entry, *which* raw words, and — the one fact
//! that separates a copier gap from an already-dangling word — whether the address the word names is
//! inside a region the pass is about to drop, a region that is still installed, or **no live region
//! at all**.
//!
//! That is what this module prints, on the `BEAM_RT_STORE_TRACE` channel (`[rt-store]`, off by
//! default — [`crate::debug_trace::store`]). With the variable unset every entry point here returns
//! on its first cached-`OnceLock` test, so a production run pays nothing and **no behaviour depends
//! on any of it**: nothing here decides, moves, retains or frees a word.
//!
//! # What is printed
//!
//! * **the attestation** — the move's attributed owner (`Family(family, slot)` / `CallerSlot(i)` /
//!   `OutsideCorpus`) and the word that slot holds, or the walk's unknown header with its subtag;
//! * **the running process's whole dictionary** — every `(key, value)` pair, each word with its raw
//!   address, where that address lives, and the rendered term (through the registered `io:format/2`
//!   printer, [`Runtime::format_term_w`] — never a second printer). The dictionary is the family the
//!   two measurements named, and one entry per line is what makes the offending entry identifiable
//!   rather than merely present;
//! * **the hit chain** — the path of term words from the attributed root down to the first word whose
//!   address stands in a candidate range, each link with its own location and its raw words;
//! * **the address classification** — `candidate range #i` (a store range the pass would drop), a
//!   live store/bif region, a live **arena** region, or **no live region at all** (a dangling word,
//!   which is what a region that was already released leaves behind).
//!
//! # Why reading a word here cannot fault
//!
//! Two words are never read as a term: an address outside every **live** region (a word that names a
//! released region is *reported*, never dereferenced — its region may have been unmapped), and a
//! word whose own graph the reachability walk cannot complete (rendered as "not legible" instead of
//! handed to the printer). Every read goes through [`read_at`], which copies the word out of a region
//! slice the runtime still holds ([`Runtime::for_each_off_heap_region`] and the arena's
//! [`MinimalContext::for_each_heap_region`]), so this module needs no `unsafe` of its own: the one
//! `beam_term` call that does — [`beam_term::layout::for_each_child_slot`] — carries its `SAFETY`
//! rationale at the site.
//!
//! [`MinimalContext::for_each_heap_region`]: beam_engine::MinimalContext::for_each_heap_region

// One narrow `unsafe` block, and only for the term-layout child enumeration: the object's header
// word was read out of a region the runtime still holds, so the address it came from heads that
// object — the contract `beam_term::layout::for_each_child_slot` states.
#![allow(unsafe_code)]

use core::mem::{align_of, size_of};

use beam_term::Eterm;

use super::compact::MoveSlots;
use super::MoveHitOwner;
use crate::debug_trace;
use crate::eval::reclaim::{self, Reach, Walk};
use crate::runtime::Runtime;

/// How many dictionary entries are printed before the report says it truncated.
const MAX_DICT_ENTRIES: usize = 64;
/// How many raw words of an object are printed per chain link.
const RAW_WORDS: usize = 8;
/// The chain search's depth bound.
const MAX_CHAIN_DEPTH: usize = 48;
/// The chain search's visit budget (a forged cycle must not spin).
const CHAIN_BUDGET: usize = 4096;
/// How many corpus slots the refusal attribution examines (and prints).
const MAX_REFUSING_ROOTS: usize = 64;

/// The verification's hit: which corpus slot reproduced it, and what that slot's graph holds.
pub(crate) fn move_hit(
    runtime: &mut Runtime,
    slots: &MoveSlots<'_>,
    candidates: &[(usize, usize)],
    owner: MoveHitOwner,
) {
    if !debug_trace::store_enabled() {
        return;
    }
    debug_trace::store(format_args!(
        "reclaim: store-move verification hit — attributed owner={owner:?}"
    ));
    let root = attributed_word(runtime, slots, owner);
    match root {
        Some(word) => {
            let address = term_address(word);
            debug_trace::store(format_args!(
                "  holder word={:#x} at {}",
                word.raw(),
                describe(runtime, candidates, address)
            ));
            trace_chain(runtime, word, candidates);
        }
        None => {
            debug_trace::store(format_args!(
                "  no corpus slot reproduces the hit (the holder is outside the corpus)"
            ));
        }
    }
    trace_dictionary(runtime, candidates);
    trace_candidates(candidates);
}

/// The pass's own (or the verification's) `Walk::UnknownHeader` refusal: the word, the header it
/// read as an object's first word, and the raw words standing at that address.
pub(crate) fn walk_decline(
    runtime: &mut Runtime,
    verdict: &Walk,
    candidates: &[(usize, usize)],
    slots: Option<&MoveSlots<'_>>,
) {
    if !debug_trace::store_enabled() {
        return;
    }
    let Walk::UnknownHeader { header, term } = verdict else {
        return;
    };
    let word = Eterm::from_raw(*term);
    let address = term_address(word);
    debug_trace::store(format_args!(
        "reclaim: reachability walk met an unknown boxed header={header:#x} subtag={:#04x} \
         term={term:#x} at {}",
        header & beam_term::tag::TAG_HEADER_MASK,
        describe(runtime, candidates, address)
    ));
    if let Some(address) = address {
        let width = size_of::<Eterm>();
        for offset in 0..RAW_WORDS {
            let Some(word) = read_at(runtime, address + offset * width) else {
                break;
            };
            debug_trace::store(format_args!(
                "  [{offset}] {:#018x} (as a term: {})",
                word.raw(),
                describe_term(runtime, candidates, word)
            ));
        }
        trace_chain(runtime, word, candidates);
    }
    attribute_decline(runtime, *header, slots);
    trace_dictionary(runtime, candidates);
    trace_candidates(candidates);
}

/// Attributes the refusal to **the corpus slot whose own graph meets it** — the finding a fix needs.
///
/// The walk is one descent over the whole corpus, so a refusal names only the header; this re-runs
/// it from each corpus slot on its own and reports every slot that meets the same header, with the
/// path from that slot down to the offending object. A slot whose graph refuses on a *different*
/// word is reported too (marked), because two corrupt graphs in one window is itself a finding.
///
/// Cost: one descent per corpus slot, paid only on the refusal path **and** only when
/// `BEAM_RT_STORE_TRACE` is set; the report is capped at [`MAX_REFUSING_ROOTS`] slots.
fn attribute_decline(
    runtime: &mut Runtime,
    header: usize,
    slots: Option<&MoveSlots<'_>>,
) {
    let mut reported = 0usize;
    let mut examined = 0usize;
    let no_candidates: [(usize, usize); 0] = [];
    if let Some(slots) = slots {
        for (index, slot) in slots.values().iter().enumerate() {
            examined += 1;
            if report_refusal(runtime, "CallerSlot", index, *slot, header, &no_candidates) {
                reported += 1;
            }
        }
    }
    let mut rows: Vec<(String, usize, Eterm)> = Vec::new();
    let _ = runtime.for_each_root_family_mut(&mut |family, words| {
        for (index, word) in words.iter().enumerate() {
            rows.push((format!("{family:?}"), index, *word));
        }
        Ok(())
    });
    for (name, index, word) in rows.iter().take(MAX_REFUSING_ROOTS) {
        examined += 1;
        if report_refusal(runtime, name, *index, *word, header, &no_candidates) {
            reported += 1;
        }
    }
    debug_trace::store(format_args!(
        "  {reported} of {examined} corpus slot(s) refuse on the walk (at most \
         {MAX_REFUSING_ROOTS} family slots were examined)"
    ));
}

/// Walks one corpus slot and prints it when its graph refuses. Answers whether it did.
fn report_refusal(
    runtime: &mut Runtime,
    family: &str,
    index: usize,
    word: Eterm,
    header: usize,
    candidates: &[(usize, usize)],
) -> bool {
    let mut sink = NullSink;
    let Walk::UnknownHeader {
        header: met,
        term,
    } = reclaim::walk(core::slice::from_ref(&word), &mut sink)
    else {
        return false;
    };
    let same = met == header;
    debug_trace::store(format_args!(
        "  refusing corpus slot: {family}[{index}] word={:#x} at {} -> header={met:#x} subtag={:#04x} \
         term={term:#x}{}",
        word.raw(),
        describe_term(runtime, candidates, word),
        met & beam_term::tag::TAG_HEADER_MASK,
        if same { "" } else { " (a DIFFERENT header)" }
    ));
    let mut budget = CHAIN_BUDGET;
    if let Some(path) = path_to_unknown_header(runtime, word, MAX_CHAIN_DEPTH, &mut budget) {
        for (depth, step) in path.iter().enumerate() {
            let address = term_address(*step);
            debug_trace::store(format_args!(
                "      [{depth}] word={:#x} at {}",
                step.raw(),
                describe(runtime, candidates, address)
            ));
        }
    }
    true
}

/// The `RootFamily` variant's own `Debug` name is the one spelling every report uses above.
/// The word the move's attribution named: the caller slot's current value, or the family slot's.
///
/// `None` for [`MoveHitOwner::OutsideCorpus`] (no slot reproduces the hit — the finding itself) and
/// for a family slot that no longer exists in the slice it was attributed to.
fn attributed_word(
    runtime: &mut Runtime,
    slots: &MoveSlots<'_>,
    owner: MoveHitOwner,
) -> Option<Eterm> {
    match owner {
        MoveHitOwner::CallerSlot(index) => slots.values().get(index).copied(),
        MoveHitOwner::Family(family, slot) => {
            let mut found = None;
            // The family walk answers `Result`, which this visitor never fails; a family that is
            // not the attributed one, or a slot past its end, contributes nothing.
            let _ = runtime.for_each_root_family_mut(&mut |visited, words| {
                if found.is_none() && visited == family {
                    found = words.get(slot).copied();
                }
                Ok(())
            });
            found
        }
        MoveHitOwner::OutsideCorpus => None,
    }
}

/// The path of term words from `root` to the first word whose address stands in a candidate range,
/// printed link by link. A root that names no such word prints that fact instead.
fn trace_chain(runtime: &mut Runtime, root: Eterm, candidates: &[(usize, usize)]) {
    let mut budget = CHAIN_BUDGET;
    match path_to_hit(runtime, root, candidates, MAX_CHAIN_DEPTH, &mut budget) {
        Some(path) => {
            debug_trace::store(format_args!("  hit chain ({} link(s)):", path.len()));
            for (depth, word) in path.iter().enumerate() {
                let address = term_address(*word);
                debug_trace::store(format_args!(
                    "    [{depth}] word={:#x} at {}",
                    word.raw(),
                    describe(runtime, candidates, address)
                ));
                if let Some(address) = address {
                    trace_raw_words(runtime, address);
                }
            }
        }
        None => debug_trace::store(format_args!(
            "  the root's graph names no word inside a candidate range"
        )),
    }
}

/// Prints the raw words at `address` (the object's own words, up to [`RAW_WORDS`]).
fn trace_raw_words(runtime: &mut Runtime, address: usize) {
    let width = size_of::<Eterm>();
    for offset in 0..RAW_WORDS {
        let Some(word) = read_at(runtime, address + offset * width) else {
            break;
        };
        debug_trace::store(format_args!(
            "        +{offset} {:#018x}{}",
            word.raw(),
            child_offset_note(runtime, address, offset, word)
        ));
    }
}

/// A one-word annotation for a raw word: whether it is a term word naming a live region.
fn child_offset_note(
    runtime: &mut Runtime,
    _address: usize,
    _offset: usize,
    word: Eterm,
) -> String {
    match term_address(word) {
        Some(address) => format!(
            " -> {}",
            describe(runtime, &[], Some(address))
        ),
        None => String::from(" (not a container term word)"),
    }
}

/// The running process's **whole** dictionary: every pair, its words, their location and the
/// rendered term.
fn trace_dictionary(runtime: &mut Runtime, candidates: &[(usize, usize)]) {
    // Every **live** process, not just the one that is running: the `ProcessDict` family is visited
    // per pid, so a slot index alone cannot say whose dictionary held the word.
    let pids: Vec<beam_process::ProcessId> = runtime.processes.pids().collect();
    let mut printed = 0usize;
    for pid in pids {
        let entries: Vec<(Eterm, Eterm)> = match runtime.processes.get(pid) {
            Some(process) => process.dict().entries().collect(),
            None => continue,
        };
        if entries.is_empty() {
            continue;
        }
        debug_trace::store(format_args!(
            "  process dictionary of {pid:?}: {} entr{}",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        ));
        printed += entries.len();
        for (index, (key, value)) in entries.iter().take(MAX_DICT_ENTRIES).enumerate() {
            for (what, word) in [("key", *key), ("value", *value)] {
                debug_trace::store(format_args!(
                    "    [{index}].{what} word={:#x} at {} rendered={}",
                    word.raw(),
                    describe_term(runtime, candidates, word),
                    render(runtime, word)
                ));
            }
        }
        if entries.len() > MAX_DICT_ENTRIES {
            debug_trace::store(format_args!(
                "    … {} further entries of {pid:?} not printed",
                entries.len() - MAX_DICT_ENTRIES
            ));
        }
    }
    if printed == 0 {
        debug_trace::store(format_args!("  no live process holds a dictionary entry"));
    } else {
        debug_trace::store(format_args!("  {printed} dictionary entr(y|ies) in total"));
    }
}

/// Prints the candidate ranges themselves, so a hit address can be placed against them.
fn trace_candidates(candidates: &[(usize, usize)]) {
    debug_trace::store(format_args!(
        "  candidate ranges ({}):",
        candidates.len()
    ));
    for (index, (lo, hi)) in candidates.iter().enumerate() {
        debug_trace::store(format_args!(
            "    #{index} [{lo:#x},{hi:#x}) {} word(s)",
            (hi.saturating_sub(*lo)) / size_of::<Eterm>()
        ));
    }
}

/// The term's address and its location, for a [`term_address`] result.
fn describe(runtime: &mut Runtime, candidates: &[(usize, usize)], address: Option<usize>) -> String {
    match address {
        Some(address) => format!("{address:#x} ({})", locate(runtime, candidates, address)),
        None => String::from("no address (an immediate or a literal word)"),
    }
}

/// A word's address and its location, or a note that it carries none.
fn describe_term(runtime: &mut Runtime, candidates: &[(usize, usize)], word: Eterm) -> String {
    describe(runtime, candidates, term_address(word))
}

/// Where `address` lives: a candidate range, a live region, or nothing at all.
fn locate(runtime: &mut Runtime, candidates: &[(usize, usize)], address: usize) -> String {
    if let Some((index, (lo, hi))) = candidates
        .iter()
        .enumerate()
        .find(|(_, (lo, hi))| address >= *lo && address < *hi)
    {
        return format!("candidate range #{index} [{lo:#x},{hi:#x}) — a region the pass would drop");
    }
    if in_store_region(runtime, address) {
        return String::from("a live store/bif region (not a candidate)");
    }
    if read_word_at(runtime, address).is_some() {
        return String::from("a live arena region");
    }
    String::from("NO live region — the word dangles (its region was released)")
}

/// Whether `address` stands inside one of the runtime's live **store** regions (the term store, its
/// retired generations, the pooled BIF heaps and the `system_info` scratch heap).
fn in_store_region(runtime: &mut Runtime, address: usize) -> bool {
    let width = size_of::<Eterm>();
    let mut found = false;
    runtime.for_each_off_heap_region(|words| {
        if found || words.is_empty() {
            return;
        }
        let base = words.as_ptr() as usize;
        found = address >= base && address < base + words.len() * width;
    });
    found
}

/// The word at `address`, read out of a region slice the runtime still holds — the store's regions
/// first, then the engine arena's. `None` when no **live** region contains the address: a released
/// region may have been unmapped, so a word naming one is reported, never dereferenced.
fn read_word_at(runtime: &mut Runtime, address: usize) -> Option<Eterm> {
    let width = size_of::<Eterm>();
    let mut found: Option<Eterm> = None;
    runtime.for_each_off_heap_region(|words| {
        take_word(words, address, width, &mut found);
    });
    if found.is_none() {
        runtime.arena.for_each_heap_region(|words| {
            take_word(words, address, width, &mut found);
        });
    }
    found
}

/// Copies the word at `address` out of `words`, when it lies inside it and nothing was found yet.
fn take_word(words: &[Eterm], address: usize, width: usize, found: &mut Option<Eterm>) {
    if found.is_some() || words.is_empty() {
        return;
    }
    let base = words.as_ptr() as usize;
    if address >= base && address < base + words.len() * width {
        *found = Some(words[(address - base) / width]);
    }
}

/// The term's address, when `word` is a container term word naming an aligned, non-null address.
fn term_address(word: Eterm) -> Option<usize> {
    if beam_term::literal::is_literal(word)
        || !beam_term::tag::is_term_word(word)
        || !(beam_term::boxed::is_boxed_term(word) || beam_term::prelude::is_list(word))
    {
        return None;
    }
    let address = beam_term::list::ptr_val(word) as usize;
    (address != 0 && address % align_of::<Eterm>() == 0).then_some(address)
}

/// The word at `address`, read out of a region slice the runtime still holds — `None` when no live
/// region contains the address (the caller reports that rather than dereferencing it).
fn read_at(runtime: &mut Runtime, address: usize) -> Option<Eterm> {
    if address % align_of::<Eterm>() != 0 {
        return None;
    }
    read_word_at(runtime, address)
}

/// The first path of term words from `word` down to the object whose header word this module cannot
/// classify — the chain a "which object is corrupt" question needs.
fn path_to_unknown_header(
    runtime: &mut Runtime,
    word: Eterm,
    depth: usize,
    budget: &mut usize,
) -> Option<Vec<Eterm>> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let address = term_address(word)?;
    if !beam_term::prelude::is_list(word) {
        match read_at(runtime, address) {
            Some(header) => {
                if !is_known_header(header) {
                    return Some(vec![word]);
                }
            }
            None => return Some(vec![word]),
        }
    }
    if depth == 0 {
        return None;
    }
    for child in child_words(runtime, word, address) {
        if let Some(mut path) = path_to_unknown_header(runtime, child, depth - 1, budget) {
            path.insert(0, word);
            return Some(path);
        }
    }
    None
}

/// Whether `header`'s subtag is one of the shapes the constructor set writes — the same arm list
/// [`crate::eval::reclaim::walk`] enumerates.
fn is_known_header(header: Eterm) -> bool {
    matches!(
        header.raw() & beam_term::tag::TAG_HEADER_MASK,
        beam_term::tag::BIN_REF_SUBTAG
            | beam_term::tag::ARITYVAL_SUBTAG
            | beam_term::tag::RECORD_SUBTAG
            | beam_term::tag::FUN_SUBTAG
            | beam_term::tag::MAP_SUBTAG
            | beam_term::tag::SUB_BITS_SUBTAG
            | beam_term::tag::FLOAT_SUBTAG
            | beam_term::tag::POS_BIG_SUBTAG
            | beam_term::tag::NEG_BIG_SUBTAG
            | beam_term::tag::REF_SUBTAG
            | beam_term::tag::HEAP_BITS_SUBTAG
            | beam_term::tag::EXTERNAL_PID_SUBTAG
            | beam_term::tag::EXTERNAL_PORT_SUBTAG
            | beam_term::tag::EXTERNAL_REF_SUBTAG
    )
}

/// The first path of term words from `word` to a word standing inside a candidate range.
fn path_to_hit(
    runtime: &mut Runtime,
    word: Eterm,
    candidates: &[(usize, usize)],
    depth: usize,
    budget: &mut usize,
) -> Option<Vec<Eterm>> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let address = term_address(word)?;
    if candidates
        .iter()
        .any(|(lo, hi)| address >= *lo && address < *hi)
    {
        return Some(vec![word]);
    }
    if depth == 0 {
        return None;
    }
    for child in child_words(runtime, word, address) {
        if let Some(mut path) = path_to_hit(runtime, child, candidates, depth - 1, budget) {
            path.insert(0, word);
            return Some(path);
        }
    }
    None
}

/// The child term words of `word`, enumerated the way the reachability walk enumerates them.
fn child_words(runtime: &mut Runtime, word: Eterm, address: usize) -> Vec<Eterm> {
    let width = size_of::<Eterm>();
    if beam_term::prelude::is_list(word) {
        // A cons cell: its `car` and `cdr` are the two words at the address.
        return [read_at(runtime, address), read_at(runtime, address + width)]
            .into_iter()
            .flatten()
            .collect();
    }
    let Some(header) = read_at(runtime, address) else {
        return Vec::new();
    };
    let mut offsets: Vec<usize> = Vec::new();
    match header.raw() & beam_term::tag::TAG_HEADER_MASK {
        beam_term::tag::ARITYVAL_SUBTAG
        | beam_term::tag::RECORD_SUBTAG
        | beam_term::tag::FUN_SUBTAG
        | beam_term::tag::MAP_SUBTAG
        | beam_term::tag::SUB_BITS_SUBTAG => {
            let ptr = address as *const Eterm;
            // SAFETY: `address` was just read out of a live region, so it heads the object whose
            // header word `header` is — the contract `for_each_child_slot` asks for. The visitor
            // only pushes a word offset, and no pass runs while this diagnostic is on the stack (it
            // is called from the refusal path, with `&mut Runtime` held), so nothing relocates.
            unsafe {
                beam_term::layout::for_each_child_slot(header, ptr, |offset| offsets.push(offset));
            }
        }
        _ => {}
    }
    offsets
        .into_iter()
        .filter_map(|offset| read_at(runtime, address + offset * width))
        .collect()
}

/// Renders `word` through the registered `io:format/2` printer, but **only** when its whole graph is
/// legible to the reachability walk — a corrupt graph is reported instead of being handed to the
/// printer (which would dereference it).
fn render(runtime: &mut Runtime, word: Eterm) -> String {
    let mut sink = NullSink;
    match reclaim::walk(&[word], &mut sink) {
        Walk::Complete => match runtime.format_term_w(word, None) {
            Ok(text) => text.trim_end().to_string(),
            Err(error) => format!("<unprintable: {error}>"),
        },
        Walk::Declined(reason) => format!("<its graph is not legible: {reason}>"),
        Walk::UnknownHeader { header, .. } => {
            format!("<its graph holds an unknown boxed header {header:#x}>")
        }
    }
}

/// A sink that asks for nothing: it exists so [`render`] can run the **one** walker as a
/// legibility test rather than re-implementing a traversal.
struct NullSink;

impl Reach for NullSink {}
