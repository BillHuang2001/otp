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

//! The **Rust** battery: every case of the oracle program
//! (`erl_program.rs`) re-run through the `beam_ets` public API, reduced to the
//! *identical* canonical rendering.
//!
//! The module docs of `../differential.rs` state the comparison contract; this
//! module is the single place that must obey it:
//!
//! * case names and their order match the Erlang `emit/2` calls exactly;
//! * [`render`] is a line-by-line port of the oracle's `r/1` (small integers,
//!   atoms, tuples, proper lists — nothing else is ever produced);
//! * [`Battery::srt`] / [`Battery::inorder`] mirror the oracle's `srt/1` /
//!   `inorder/1`, and the *choice* of one over the other matches too (sorted
//!   for anything whose order is not contract, in-order only for
//!   `ordered_set` iteration);
//! * a raising call is rendered through [`cap_value`]/[`cap_bool`] as `badarg`,
//!   `error:<reason>` or `ok:<rendered>` — exactly like the oracle's `cap/1`.
//!
//! Terms are built in the `beam_term::heap::Heap` test arena and results that
//! need to be *constructed* (select bodies, `update_element`) are materialised
//! on a `beam_alloc::Heap`, mirroring who owns what in the real runtime.
//!
//! The individual cases live in one submodule per group — the same groups the
//! oracle program's `emit/2` calls are written in — while this file owns the
//! shared [`Battery`] state, the canonical renderer, and the [`rust_battery`]
//! entry point that concatenates the groups in the oracle's order.

use beam_alloc::Heap as OutHeap;
use beam_common::{BeamError, Result as BeamResult};
use beam_ets::{compile, MatchContinuation, MatchSpec, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

/// Words reserved for the arena the battery builds source objects and match
/// specs in. The two 10 001-key tables dominate it: `2 × 10 001 × 3` words.
const ARENA_WORDS: usize = 1 << 18;
/// Words reserved for the `beam_alloc::Heap` that constructed results land on.
const OUT_WORDS: usize = 1 << 16;
/// The object count of the `set_large` / `ordered_large` cases.
const LARGE_N: i64 = 10001;

mod continuation;
mod counter;
mod info;
mod kind;
mod keypos;
mod large;
mod lookup;
mod matchspec;
mod multikind;
mod navigation;
mod reclamation;
mod replace;
mod access;
mod slot;

// ---------------------------------------------------------------------------
// Canonical rendering (the port of the oracle's `r/1`, `t/1`, `l/1`, `srt/1`)
// ---------------------------------------------------------------------------

/// Render `term` in the canonical form: small integers, atoms, tuples and proper
/// lists only. Any other term renders as an explicit, unmatchable marker — the
/// battery is supposed to be built out of those four classes, and a marker makes
/// a stray float/binary show up as a loud diff rather than silently rendering as
/// something plausible.
pub fn render(term: Eterm, atoms: &AtomTable) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    // SAFETY: `is_big` classifies `term` through its tag/header word.
    if unsafe { is_big(term) } {
        // SAFETY: the check above established a live bignum, so `big_parts` reads
        // its header and digit words.
        let (negative, digits) = unsafe { big_parts(term) };
        return decimal_of(negative, digits);
    }
    if is_atom(term) {
        return match atoms.name(atom_val(term)) {
            Some(name) => name.to_string(),
            None => format!("#unknown-atom{}", atom_val(term).raw()),
        };
    }
    // SAFETY: `is_tuple` classifies `term` by its tag/header word and only reads
    // further once the boxed arityval shape is established, so it is sound for
    // any value word.
    if unsafe { is_tuple(term) } {
        // SAFETY: the check above established a live tuple whose header word is a
        // valid arityval, so its arity is readable.
        let arity = unsafe { tuple_size(term) };
        let parts: Vec<String> = (0..arity)
            .map(|i| {
                // SAFETY: `i < arity == tuple_size(term)` is exactly
                // `tuple_element`'s precondition, and the tuple is alive for the
                // duration of the borrow (a table-owned or heap-owned term).
                render(unsafe { tuple_element(term, i) }, atoms)
            })
            .collect();
        return tuple_of(&parts);
    }
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_list(term) {
        let mut parts = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            // SAFETY: `is_list(cursor)` established that `cursor` is a live cons
            // cell, so both of its `[CAR, CDR]` words are initialised.
            let head = unsafe { car(cursor) };
            let tail = unsafe { cdr(cursor) };
            parts.push(render(head, atoms));
            cursor = tail;
        }
        if !is_nil(cursor) {
            parts.push(format!("|{}", render(cursor, atoms)));
        }
        return list_of(&parts);
    }
    format!("#unrenderable{:#x}", term.raw())
}

/// The oracle's `t/1`: join already-rendered parts as a tuple.
pub fn tuple_of(parts: &[String]) -> String {
    format!("{{{}}}", parts.join(","))
}
/// The oracle's `l/1`: join already-rendered parts as a list.
pub fn list_of(parts: &[String]) -> String {
    format!("[{}]", parts.join(","))
}

/// The decimal text of an integer magnitude — the port of the oracle's
/// `integer_to_list/1` for the bignum class (`r/1`'s `is_integer` clause).
///
/// `digits` are little-endian `ErtsDigit` (`u64`) limbs, least significant first;
/// the magnitude is repeatedly divided by ten, emitting one decimal digit per
/// step, which is exact for any width.
fn decimal_of(negative: bool, digits: &[u64]) -> String {
    let mut limbs = digits.to_vec();
    let mut text: Vec<u8> = Vec::new();
    while !limbs.is_empty() {
        let mut remainder: u128 = 0;
        for limb in limbs.iter_mut().rev() {
            let current = (remainder << 64) | *limb as u128;
            *limb = (current / 10) as u64;
            remainder = current % 10;
        }
        text.push(b'0' + remainder as u8);
        while matches!(limbs.last(), Some(0)) {
            limbs.pop();
        }
    }
    if text.is_empty() {
        // A zero-width magnitude never occurs (a normalised bignum has at least one
        // non-zero limb), but render it as the conventional `0` just in case.
        text.push(b'0');
    }
    let mut rendered: String = text.iter().rev().map(|byte| *byte as char).collect();
    if negative {
        rendered.insert(0, '-');
    }
    rendered
}

/// The little-endian `u64` limbs of `2^exp` — the magnitude the oracle's
/// `1 bsl exp` produces.
fn pow2(exp: u32) -> Vec<u64> {
    let mut digits = vec![0u64; (exp / 64 + 1) as usize];
    digits[(exp / 64) as usize] = 1u64 << (exp % 64);
    digits
}

/// The limbs of `2^exp + delta` for a non-negative `delta` (used to build the
/// oracle's `(1 bsl 192) + 7`-style operands).
fn pow2_plus(exp: u32, delta: u64) -> Vec<u64> {
    let mut digits = pow2(exp);
    let mut carry = delta;
    let mut index = 0usize;
    while carry != 0 {
        if index == digits.len() {
            digits.push(0);
        }
        let (sum, overflow) = digits[index].overflowing_add(carry);
        digits[index] = sum;
        carry = u64::from(overflow);
        index += 1;
    }
    digits
}

/// `true`/`false` as the oracle renders them.
pub fn bool_str(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// The oracle's `cap/1` for an integer/atom/tuple/list result.
pub fn cap_value(result: BeamResult<Eterm>, atoms: &AtomTable) -> String {
    match result {
        Ok(term) => format!("ok:{}", render(term, atoms)),
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a boolean result (`ets:update_element/3`).
pub fn cap_bool(result: BeamResult<bool>) -> String {
    match result {
        Ok(value) => format!("ok:{}", bool_str(value)),
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a term-*list* result — the multi-object kinds'
/// `ets:lookup_element/3`, whose success is the element list in stored order.
pub fn cap_terms(result: BeamResult<Vec<Eterm>>, atoms: &AtomTable) -> String {
    match result {
        Ok(terms) => format!("ok:{}", list_of(&rendered(&terms, atoms))),
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a term-*list* result whose iteration order is **not**
/// part of the contract: the rendered terms **sorted** (the oracle's
/// `cap_str/1` over `srt/1`).
pub fn cap_srt(result: BeamResult<Vec<Eterm>>, atoms: &AtomTable) -> String {
    match result {
        Ok(terms) => {
            let mut parts = rendered(&terms, atoms);
            parts.sort();
            format!("ok:{}", list_of(&parts))
        }
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a navigation result (`ets:first`/`ets:next`, the
/// crate's `Option<Eterm>`): the rendered key, `$end_of_table` for `None`, or the
/// error class.
pub fn cap_opt(result: BeamResult<Option<Eterm>>, atoms: &AtomTable) -> String {
    match result {
        Ok(Some(term)) => format!("ok:{}", render(term, atoms)),
        Ok(None) => "ok:$end_of_table".to_string(),
        Err(error) => cap_error(&error),
    }
}

/// The rendered forms of `terms` in order — the shared body of the oracle's
/// `r/1` (over a list), `srt/1` and `inorder/1`.
pub fn rendered(terms: &[Eterm], atoms: &AtomTable) -> Vec<String> {
    terms.iter().map(|t| render(*t, atoms)).collect()
}

/// The oracle's `cap(fun() -> ets:insert(T, List) end)`: a successful list
/// insert answers the atom `true`, so `Ok(())` renders as `ok:true`.
pub fn cap_insert_list(result: BeamResult<()>) -> String {
    cap_unit(result)
}

/// The oracle's `cap/1` for a call answering the atom `true` on success
/// (`ets:insert/2` over a list, `ets:give_away/3`, `ets:setopts/2`).
pub fn cap_unit(result: BeamResult<()>) -> String {
    match result {
        Ok(()) => "ok:true".to_string(),
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a call answering an integer count on success
/// (`ets:select_replace/2`): the count, or the error class.
pub fn cap_count(result: BeamResult<usize>) -> String {
    match result {
        Ok(count) => format!("ok:{count}"),
        Err(error) => cap_error(&error),
    }
}

/// The oracle's `cap/1` for a `select`/`match` `/3` or `/1` chunk: `ok:<in-order
/// results>` on success, else the error class. The continuation the oracle hands
/// back is an **opaque ref** and is never rendered, so only the materialized
/// results are (`Option<MatchContinuation>` is deliberately ignored).
pub fn cap_chunk(
    result: BeamResult<(Vec<Eterm>, Option<MatchContinuation>)>,
    atoms: &AtomTable,
) -> String {
    match result {
        Ok((results, _)) => format!("ok:{}", list_of(&rendered(&results, atoms))),
        Err(error) => cap_error(&error),
    }
}

/// The BEAM error atom a failure raises — `badarg` renders bare, any other
/// reason as `error:<reason>`, matching the oracle's `cap/1` (`catch error:E ->
/// atom_to_list(E)`).
///
/// `BeamError::as_atom` can only hand out `&'static str` names, so an **owned**
/// atom reason — this crate's typed capability gaps, whose reason is
/// `unsupported_ets: <what>` — would collapse to `error:unknown`. The raise still
/// carries the name and the oracle renders it, so it is read through the raise's
/// `ExitReason` (whose `as_atom` *borrows* the name instead of requiring
/// `'static`), keeping the gap visible instead of masked.
fn cap_error(error: &BeamError) -> String {
    let reason = match error {
        BeamError::Raise(raise) => raise.reason.as_atom(),
        other => other.as_atom(),
    };
    match reason {
        Some("badarg") => "badarg".to_string(),
        Some(reason) => format!("error:{reason}"),
        None => "error:unknown".to_string(),
    }
}
// ---------------------------------------------------------------------------
// The battery
// ---------------------------------------------------------------------------

/// The term arenas a battery run builds into.
struct Battery {
    /// Source objects, keys and match-spec terms.
    atoms: AtomTable,
    /// The arena the source terms live in (`beam_term`'s test arena).
    heap: Heap,
    /// The heap constructed results materialise on (`beam_alloc`, as in the
    /// runtime).
    out: OutHeap,
}

/// The shared state and helpers every case group builds on.
impl Battery {
    fn new() -> Battery {
        let mut atoms = AtomTable::new();
        // The match-spec executor resolves the `true`/`false` a guard produces by
        // *name* against the caller's atom table (`select::expr::bool_term`), and
        // `AtomTable::new()` seeds no standard atoms — so a battery whose table has
        // never interned them would see every guard fail (and every `select`
        // return `[]`). The runtime's global atom table always knows them; the
        // battery has to say so explicitly.
        for name in ["true", "false"] {
            atoms.intern(name);
        }
        Battery {
            atoms,
            heap: Heap::new(ARENA_WORDS),
            out: OutHeap::new(OUT_WORDS).expect("the output heap is allocatable"),
        }
    }

    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    fn tuple(&mut self, elements: &[Eterm]) -> Eterm {
        self.heap.tuple(elements).expect("the battery arena has room")
    }

    fn list(&mut self, elements: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for element in elements.iter().rev() {
            acc = self
                .heap
                .cons(*element, acc)
                .expect("the battery arena has room");
        }
        acc
    }

    /// A tuple of small integers.
    fn ints(&mut self, values: &[i64]) -> Eterm {
        let terms: Vec<Eterm> = values.iter().copied().map(make_small).collect();
        self.tuple(&terms)
    }

    /// A `{Key, Value}` object with integral elements.
    fn object(&mut self, key: i64, value: i64) -> Eterm {
        self.ints(&[key, value])
    }

    /// A bignum of `digits` (least significant first, non-empty) on the arena.
    fn big(&mut self, negative: bool, digits: &[u64]) -> Eterm {
        // SAFETY: `alloc_words` hands out exactly `big_size_words(n)` writable
        // words, all of which `write_big` fills (`digits` is non-empty everywhere
        // this is called).
        let hp = unsafe {
            self.heap
                .alloc_words(big_size_words(digits.len()))
                .expect("the battery arena has room")
        };
        // SAFETY: `hp` addresses exactly `big_size_words(len)` words.
        unsafe { write_big(hp, negative, digits) }
    }

    /// A `{Key, Value}` object whose value is a bignum.
    fn big_object(&mut self, key: i64, negative: bool, digits: &[u64]) -> Eterm {
        let value = self.big(negative, digits);
        self.tuple(&[make_small(key), value])
    }

    /// A float term on the arena (the `ordered_set` mixed-key case needs a key the
    /// renderer cannot print, so the float is only ever a *key*, never rendered).
    fn float(&mut self, value: f64) -> Eterm {
        // SAFETY: `alloc_words` hands out exactly `FLOAT_SIZE_WORDS` writable words,
        // and `write_float` fills both of them with the `[HEADER_FLONUM, bits]` block.
        let hp = unsafe {
            self.heap
                .alloc_words(FLOAT_SIZE_WORDS)
                .expect("the battery arena has room")
        };
        // SAFETY: `hp` addresses exactly `FLOAT_SIZE_WORDS` exclusively-owned words.
        unsafe { write_float(hp, value) }
    }

    /// An on-heap binary term on the arena (again only ever used as a *key*).
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        // SAFETY: `alloc_words` hands out exactly `heap_bits_size_words(n_bits)`
        // writable words for the `ErlHeapBits` block `write_binary` fills.
        let hp = unsafe {
            self.heap
                .alloc_words(heap_bits_size_words(bytes.len() * 8))
                .expect("the battery arena has room")
        };
        // SAFETY: `hp` addresses exactly `heap_bits_size_words(len * 8)` words.
        unsafe { write_binary(hp, bytes).expect("the binary fits") }
    }

    /// A flat map on the arena from `pairs` (the `#{K => V, …}` a spec guard, a
    /// spec body or an object constructs), built with the frozen
    /// `beam_term::map::write_map` the runtime uses — never a local map builder.
    fn map(&mut self, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let capacity = map_words_capacity(pairs.len());
        // SAFETY: `alloc_words(capacity)` hands back exactly the `capacity`
        // exclusively-owned, writable words `write_map`'s contract requires;
        // `write_map` initialises every word of the map it returns and the arena
        // outlives the term.
        unsafe {
            let hp = self
                .heap
                .alloc_words(capacity)
                .expect("the battery arena has room");
            write_map(hp, pairs, &self.atoms).expect("the map is writable")
        }
    }

    /// A fresh table of `kind` with the given 1-based `keypos`.
    fn table(&mut self, kind: TableKind, keypos: usize) -> Table {
        let mut options = TableOptions::new();
        options.set_kind(kind);
        if keypos != 1 {
            options.set_keypos(keypos).expect("keypos is >= 1");
        }
        Table::new(TableId::next(), options).expect("a fresh table is constructible")
    }

    /// The oracle's `r/1` for a single term.
    fn r(&self, term: Eterm) -> String {
        render(term, &self.atoms)
    }

    /// The oracle's `srt/1`: the rendered terms, sorted (order-independent).
    fn srt(&self, terms: &[Eterm]) -> String {
        let mut parts = rendered(terms, &self.atoms);
        parts.sort();
        list_of(&parts)
    }

    /// The oracle's `inorder/1`: the rendered terms in their stored order.
    fn inorder(&self, terms: &[Eterm]) -> String {
        list_of(&rendered(terms, &self.atoms))
    }

    /// `'$end_of_table'` for an exhausted navigation, else the rendered key.
    fn opt(&self, key: Option<Eterm>) -> String {
        match key {
            Some(key) => self.r(key),
            None => "$end_of_table".to_string(),
        }
    }

    /// One `{Head, Guards, Body}` clause.
    fn clause(&mut self, head: Eterm, guards: Eterm, body: Eterm) -> Eterm {
        self.tuple(&[head, guards, body])
    }

    /// The one-clause spec term for a head, guards and body.
    fn spec(&mut self, head: Eterm, guards: Eterm, body: Eterm) -> Eterm {
        let clause = self.clause(head, guards, body);
        self.list(&[clause])
    }

    /// The compiled `[{{'$1','$2'}, [{'>','$2', bound}], [true]}]` — the
    /// `select_count` guard the large cases use.
    fn count_gt_spec(&mut self, bound: i64) -> MatchSpec {
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v2, make_small(bound)]);
        let guards = self.list(&[guard]);
        let truth = self.atom("true");
        let body = self.list(&[truth]);
        let spec = self.spec(head, guards, body);
        compile(spec, &self.atoms).expect("the large-table spec compiles")
    }

    /// The oracle's `cap(fun() -> ets:select(T, Spec) end)`: a compiled-and-run
    /// select, rendered as its result list or as the error class.
    fn cap_select(&mut self, table: &Table, spec_term: Eterm) -> String {
        let spec = match compile(spec_term, &self.atoms) {
            Ok(spec) => spec,
            Err(error) => return cap_error(&error),
        };
        match table.select(&spec, &self.atoms, &mut self.out) {
            Ok(selected) => format!("ok:{}", self.inorder(&selected)),
            Err(error) => cap_error(&error),
        }
    }

    /// `ets:next/2` walking a whole `ordered_set` from `from`.
    fn walk_next(&self, table: &Table, from: Option<Eterm>) -> Vec<Eterm> {
        let mut keys = Vec::new();
        let mut cursor = from;
        while let Some(key) = cursor {
            keys.push(key);
            cursor = table
                .next(key, &self.atoms)
                .expect("ordered_set next never fails");
        }
        keys
    }

    /// `ets:prev/2` walking a whole `ordered_set` from `from`.
    fn walk_prev(&self, table: &Table, from: Option<Eterm>) -> Vec<Eterm> {
        let mut keys = Vec::new();
        let mut cursor = from;
        while let Some(key) = cursor {
            keys.push(key);
            cursor = table
                .prev(key, &self.atoms)
                .expect("ordered_set prev never fails");
        }
        keys
    }
}

/// Run the whole Rust battery: `(case_name, rendered_result)` in the oracle's
/// order.
pub fn rust_battery() -> Vec<(String, String)> {
    let mut battery = Battery::new();
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.extend(kind::kind_cases(&mut battery));
    cases.extend(keypos::keypos_cases(&mut battery));
    cases.extend(navigation::navigation_cases(&mut battery));
    cases.extend(counter::counter_cases(&mut battery));
    cases.extend(lookup::lookup_cases(&mut battery));
    cases.extend(matchspec::matchspec_cases(&mut battery));
    cases.extend(replace::replace_cases(&mut battery));
    cases.extend(continuation::continuation_cases(&mut battery));
    cases.extend(info::info_cases(&mut battery));
    cases.extend(large::large_cases(&mut battery));
    cases.extend(slot::slot_cases(&mut battery));
    cases.extend(reclamation::reclamation_cases(&mut battery));
    cases.extend(access::access_cases(&mut battery));
    cases.extend(multikind::multikind_cases(&mut battery));

    cases
}
