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

//! Limit + continuation forms of the select family: `ets:select/3`+`select/1`,
//! `ets:select_reverse/3`+`select_reverse/1`, `ets:match/3`+`match/1` and
//! `ets:match_object/3`+`match_object/1`.
//!
//! # What these forms do
//!
//! Given a compiled match spec and a `limit`, the `/3` forms walk the table's
//! objects in its iteration order, apply the spec to each (via the same
//! [`super::exec::run_clauses`] the non-limited forms use) and collect the body
//! values. As soon as `limit` results have been produced the scan **stops** and a
//! [`MatchContinuation`] is returned that can resume where it left off. When the
//! scan reaches the end of the table with fewer than `limit` results, no
//! continuation is produced.
//!
//! "Its iteration order" is [`Table::tab2list`] order — for an `ordered_set` that
//! is ascending arithmetic key order, which the traversal walks key by key (see the
//! cursor section below); for the hash kinds it is the interior's unspecified order.
//! The walk never assumes an order of its own: it asks the table for the first/next
//! (or last/prev) key, exactly like `ets:first`/`ets:next`.
//!
//! `ets:select_reverse/3`+`ets:select_reverse/1` are the **reverse-direction**
//! analogues: they walk the reverse of that order (descending key order for an
//! `ordered_set`) and follow every rule below unchanged. For the hash kinds the
//! carried direction degenerates to forward — see "The hash kinds' stored direction".
//!
//! # The direction *and* the body travel with the continuation, not with the `/1` name
//!
//! Every `/1` entry point is the **same** resumption: it continues the traversal the
//! continuation was created from. Neither the direction nor the result body is taken
//! from the function a caller resumes *through* — both are carried by the
//! [`MatchContinuation`]. Verified against the system `erl` (OTP 29.1.1 /
//! erts-17.1) on the `ordered_set` `{1}..{6}` with `S = [{'_',[],['$_']}]`:
//!
//! ```text
//! {FC, CFC} = ets:select(T, S, 3),          FC  = [{1},{2},{3}]
//! {RC, CRC} = ets:select_reverse(T, S, 3),  RC  = [{6},{5},{4}]
//! ets:select(FC)              = {[{4},{5},{6}],cont}   (FORWARD chunk 2)
//! ets:select_reverse(CFC)     = {[{4},{5},{6}],cont}   (still FORWARD!)
//! ets:select(RC)              = {[{3},{2},{1}],cont}   (REVERSE chunk 2)
//! ets:select_reverse(CRC)     = {[{3},{2},{1}],cont}   (still REVERSE!)
//! {_, MC} = ets:match(T, {'$1','_'}, 3),   MC  = [[1],[2],[3]]
//! ets:match(MC)               = {[[4],[5],[6]],cont}   (the BINDINGS body)
//! ets:select(MC)              = {[[4],[5],[6]],cont}   (bindings, not objects!)
//! ets:select_reverse(MC)      = {[[4],[5],[6]],cont}   (still FORWARD)
//! ets:match_object(MC)        = {[[4],[5],[6]],cont}   (the carried body wins)
//! {_, SC} = ets:select(T, S, 3),
//! ets:match(SC)               = {[{4,a},{5,a},{6,a}],cont}  (the '$_' body)
//! ```
//!
//! A resumed chunk is also reproducible: a continuation is an immutable value in the
//! oracle (`ets:select(C)` returns the same chunk however often it is called, and
//! re-resuming a finished continuation keeps yielding the bare `'$end_of_table'`),
//! which is why this crate takes every continuation by `&` and never mutates one.
//! Concretely that is why all four `/1` entry points route through one shared
//! `resume`: it reruns the continuation's own `spec` from its own
//! [`Cursor`] in its own `reverse` order, and the entry point's name is never
//! consulted.
//!
//! # A stale, foreign or forged continuation is `badarg`
//!
//! `ets:select/1` and its siblings raise `error:badarg` when the continuation's
//! table can no longer be resolved. Verified against the system `erl`:
//!
//! * a continuation whose table was **deleted** (`ets:delete/1`) ⇒ `badarg`;
//! * a continuation issued for a **`named_table`** and resumed after the table was
//!   `ets:rename/2`d ⇒ `badarg` — the oracle's continuation stores the **name atom**
//!   for a named table (its first element is `named1`, a `#Ref<…>` for an unnamed
//!   one), so the old name no longer resolves. A continuation from an **unnamed**
//!   table survives a rename (it stores the runtime id);
//! * a forged or wrong-shaped term (`garbage`, `{foo,bar,baz}`, `ets:all()`) ⇒
//!   `badarg`. The one special case is the atom `'$end_of_table'`, which is
//!   idempotent (`ets:select('$end_of_table')` ⇒ `'$end_of_table'`); that is the BIF
//!   bridge's rendering rule, not a traversal, so it does not appear here.
//!
//! This crate models the rule with the continuation's **identity**: the table's
//! [`TableId`] plus — for a `named_table` only — the name it was issued under. A
//! forged term cannot exist at all, because [`MatchContinuation`] is opaque and only
//! the `/3` methods can build one.
//!
//! The chunking rule reproduced from the oracle (verified against OTP 29 /
//! erts-17.1, `erl -noshell -eval`):
//!
//! * `limit == 0` ⇒ `badarg`.
//! * **Exactly `limit` results produced ⇒ a continuation is returned, even when
//!   the limit lands on the table's last object.** The oracle emits a
//!   continuation in that case too; the *next* resume then yields
//!   `([], '$end_of_table')`.
//! * Fewer than `limit` results and the scan reached the end ⇒ no continuation
//!   (`([], None)`, or `(results, None)`).
//! * An empty table, or a scan with no match, ⇒ `([], None)`.
//! * `limit >= table size` with the traversal finishing first ⇒ `(all, None)`.
//! * Resuming a continuation whose cursor is already past the end ⇒ `([], None)`.
//! * [`Table::select_continue`]/[`Table::match_continue`]/[`Table::match_object_continue`]
//!   resume with the **same** `limit` the
//!   originating `/3` used (the continuation carries it), so a `limit`-sized
//!   chunk is reproduced exactly.
//!
//! Concretely, on a 5-object `ordered_set` `{1,a}`…`{5,e}` with the spec
//! `[{'$1',[],['$1']}]` and `limit = 2` the chunks are
//! `[{1,a},{2,b}]`+cont · `[{3,c},{4,d}]`+cont · `([{5,e}], None)`; on a 4-object
//! table limit 2 gives `2`+cont · `2`+cont · `([], None)` (the oracle renders that
//! last one as the *bare* atom `'$end_of_table'`). `ets:select_reverse/3` with the
//! same `limit` chunks the same table as `[{5,e},{4,d}]`+cont ·
//! `[{3,c},{2,b}]`+cont · `([{1,a}], None)` (verified against the system `erl`).
//!
//! # Rendering handoff (the BIF's job, not this crate's)
//!
//! `Option::None` means the oracle's `'$end_of_table'` term. The BIF bridge must
//! render it as the **bare atom** when the accompanying `results` are empty and as
//! the 2-tuple `{results, '$end_of_table'}` otherwise (that is exactly why the
//! 4-object example's final chunk is the bare atom while the 5-object example's
//! is a tuple). A `Some(MatchContinuation)` is wrapped as `{results, ContTerm}`,
//! where the bridge maps the opaque [`MatchContinuation`] to whatever internal
//! term it hands back to `ets:select/1` & co. This crate never builds that term.
//!
//! # The cursor: a key anchor for the ordered kinds, an ordinal for the hash kinds
//!
//! Where a continuation resumes depends on the table's **kind**, because that is what
//! the two ERTS interiors do:
//!
//! * **`ordered_set`** — the continuation stores the **last key its chunk yielded**
//!   and the resume is the *range* "the first stored key strictly after (forward) /
//!   before (reverse) that anchor": the interior's `next_from(anchor, exclusive)` /
//!   `prev_from(anchor, exclusive)`, reached through [`Table::next`]/[`Table::prev`].
//!   The anchor is a **range bound, not a lookup**, so it need not still be present.
//!   This is the oracle's key cursor, verified on the system `erl` (OTP 29.1.1 /
//!   erts-17.1): on an `ordered_set` `{1}..{10}` with `limit = 4`, chunk 1 is
//!   `[1,2,3,4]`; deleting `2` **and** inserting `0` before resuming still yields
//!   `[5,6,7,8]` (the inserted `0` is not returned), and deleting the **anchor** `4`
//!   itself yields the same `[5,6,7,8]`. The reverse mirror matches:
//!   `ets:select_reverse(T, S, 3)` ⇒ `[10,9,8]`, delete `10`, resume ⇒ `[7,6,5]`.
//!   A mutation landing between chunks therefore cannot skip or repeat an object
//!   relative to the oracle. (`tests/ets_continuation_direction.rs` cross-checks
//!   every one of those probes against the live oracle.)
//! * **the hash kinds** (`set`/`bag`/`duplicate_bag`) — ERTS carries a
//!   **bucket/index cursor** (`erl_db_hash.c:2310-2313`), not a key, and this crate
//!   documents the hash iteration order as an interior freedom (it is not the
//!   oracle's order anyway), so no key-anchored rule could match ERTS here. Their
//!   continuation therefore stores an **ordinal into the table's iteration order**. A
//!   mutation landing at or behind that ordinal shifts the resume, where the oracle's
//!   bucket cursor would not: that divergence is the *only* one, it is **never a typed
//!   error and never a wrong result** for a walk with no mutation between chunks (the
//!   159-case differential battery and the whole continuation suite hold the table
//!   fixed between chunks and agree everywhere).
//!
//! Both cursor shapes are bounds-checked, so a stale ordinal never reads out of
//! bounds — it just resumes at a possibly different object.
//!
//! # The hash kinds' stored direction degenerates to forward
//!
//! ERTS's hash interior has `last`/`prev` **equal** to `first`/`next`
//! (`erl_db_hash.c:853-856`) and `db_select_reverse_hash` delegates to the same chunk
//! function as `db_select_hash` (`:2310-2313`), so for `set`/`bag`/`duplicate_bag` the
//! oracle's `ets:select_reverse` returns exactly the same list in the same order as
//! `ets:select` — per-key object order included, chunked and unchunked. The limit
//! forms reproduce that rule: on a hash kind the carried `reverse` flag is ignored and
//! both the `/3` chunks and the `/1` resumes are the forward traversal. Only
//! `ordered_set`, the crate's one ordered kind, actually reverses.
use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::exec::{run_clauses, spec_for_pattern, BodyKind};
use super::MatchSpec;
use crate::error::badarg;
use crate::table::{Table, TableId};

/// Where a resumed traversal picks up from.
///
/// The variant is chosen by the **table kind** at every construction site — the two
/// live cursors are never mixed: [`Cursor::Anchor`] for an `ordered_set`,
/// [`Cursor::Position`] for the hash kinds (`set`/`bag`/`duplicate_bag`). A
/// continuation is identity-checked against its table before its cursor is consumed,
/// and a table's kind never changes after creation, so the stored variant always
/// matches the table it is resumed against.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cursor {
    /// The transient "first call" cursor a `/3` entry point passes: begin the
    /// traversal at the table's own edge ([`Table::first`] / [`Table::last`]). It is
    /// never stored in a returned continuation, because a returned continuation
    /// always follows a produced chunk.
    Begin,
    /// An `ordered_set` resume anchor: the **last key the previous chunk yielded**.
    /// The resume is the *range* strictly after (forward) / before (reverse) that
    /// key, so it survives the anchor's own deletion — the oracle's key cursor, not a
    /// lookup.
    Anchor(Eterm),
    /// A hash kind's resume ordinal into the table's iteration order (their ERTS
    /// cursor is a bucket index, not a key — see the module docs).
    Position(usize),
}

/// An opaque, resumable traversal position for the `ets:select/3` family.
///
/// Constructed only by the `/3` methods; its fields are private so a caller can
/// never fabricate one. It carries the identity of the table it belongs to, the
/// compiled spec it is running, the limit to reuse on resume, the direction of the
/// traversal (forward for `ets:select/3`, reverse for `ets:select_reverse/3` — a
/// no-op for the hash kinds, whose reverse order *is* their forward order) and the
/// [`Cursor`] at which the next resume continues.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchContinuation {
    /// The id of the table the traversal belongs to (`Table::id`). A continuation
    /// offered to a *different* table is rejected with `badarg` rather than being
    /// applied to unrelated objects.
    table_id: TableId,
    /// The name the traversal's table was registered under, for a **`named_table`**
    /// only (`None` for every other table). The oracle's continuation stores the
    /// name atom for a named table, so a `rename` makes the previously-issued
    /// continuations unresolvable; carrying the name reproduces that `badarg`.
    name: Option<Atom>,
    /// The compiled spec the traversal runs (cloned from the originating `/3`
    /// call, so a `/1` resume needs no spec argument). It also carries the result
    /// body, which is why a `match/3` continuation resumed through `ets:select/1`
    /// still yields the bound variables.
    spec: MatchSpec,
    /// The limit the originating `/3` call used (reused by `/1`).
    limit: usize,
    /// Where the next resume continues: a key anchor for an `ordered_set`, an
    /// iteration-order ordinal for the hash kinds (see [`Cursor`] and the module
    /// docs).
    cursor: Cursor,
    /// Whether the traversal walks the table's objects in **reverse**
    /// [`Table::tab2list`] order (`ets:select_reverse/3`, `true`) or forward
    /// (`ets:select/3`, `false`). A forward and a reverse `ordered_set` walk start at
    /// opposite table edges, so the direction must survive a `/1` resume — and it
    /// does, whatever `/1` name is used. The hash kinds ignore it: their `last`/`prev`
    /// *are* their `first`/`next`, so their reverse traversal is their forward one.
    reverse: bool,
}

impl Table {
    /// The shared traversal both the `/3` forms and their `/1` resumptions run: walk
    /// from `start` in `reverse` order, produce at most `limit` results, and report
    /// either the [`Cursor`] to resume at (the oracle returns a continuation whenever
    /// exactly `limit` results were produced) or `None` when the walk reached the
    /// end.
    ///
    /// Which walk runs is decided by the **table kind**: a key-anchored range walk for
    /// `ordered_set` ([`Self::traverse_ordered`]), an ordinal walk over the iteration
    /// order for the hash kinds ([`Self::traverse_positional`]). The module docs state
    /// the exact semantics of each and the oracle behaviour they reproduce.
    ///
    /// `limit` must be non-zero (the `/3` entry points reject `0` with `badarg`);
    /// the `/1` entry points always carry a limit their originating `/3` already
    /// validated.
    fn traverse(
        &self,
        spec: &MatchSpec,
        limit: usize,
        start: &Cursor,
        reverse: bool,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<Cursor>)> {
        if self.kind().is_ordered() {
            self.traverse_ordered(spec, limit, start, reverse, atoms, heap)
        } else {
            self.traverse_positional(spec, limit, start, atoms, heap)
        }
    }

    /// The `ordered_set` traversal: a **key-anchored** range walk.
    ///
    /// `start` is [`Cursor::Begin`] for a `/3` call — the walk begins at
    /// [`Table::first`] (forward) or [`Table::last`] (reverse) — or
    /// [`Cursor::Anchor(last_key)`](Cursor::Anchor) for a resume, where it begins at
    /// the first stored key strictly after (forward) or before (reverse) the anchor.
    /// That range is [`Table::next`]/[`Table::prev`], which for an ordered kind read
    /// their argument as a bound and therefore need the anchor to be *present* no
    /// more — the oracle's key cursor.
    ///
    /// Each key is read with [`Table::lookup`] (an `ordered_set` stores at most one
    /// object per key, so that is the single object the key names) and the carried
    /// spec is applied to it.
    ///
    /// A continuation is returned exactly when `limit` results were produced — the
    /// anchor being the key that produced the `limit`-th one — even when that key is
    /// the table's last (reverse: first) one, whose resume then finds nothing.
    fn traverse_ordered(
        &self,
        spec: &MatchSpec,
        limit: usize,
        start: &Cursor,
        reverse: bool,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<Cursor>)> {
        // The first key this call considers. `Cursor::Position` cannot reach an
        // ordered table (the cursor kind is chosen by the table kind, and a table's
        // kind never changes), so it falls in with `Begin`.
        let mut key = match start {
            Cursor::Anchor(anchor) => {
                if reverse {
                    self.prev(*anchor, atoms)?
                } else {
                    self.next(*anchor, atoms)?
                }
            }
            Cursor::Begin | Cursor::Position(_) => {
                if reverse {
                    self.last(atoms)
                } else {
                    self.first(atoms)
                }
            }
        };
        let mut results = Vec::new();
        while let Some(current) = key {
            for object in self.lookup(current, atoms) {
                if let Some(value) = run_clauses(spec, object, atoms, heap)? {
                    results.push(value.into_term(atoms)?);
                }
            }
            if results.len() >= limit {
                // The oracle returns a continuation whenever it produced exactly
                // `limit` results — even when the limit landed on the table's last
                // (reverse: first) object — so the anchor is the key that produced
                // the final result.
                return Ok((results, Some(Cursor::Anchor(current))));
            }
            key = if reverse {
                self.prev(current, atoms)?
            } else {
                self.next(current, atoms)?
            };
        }
        // The walk reached the table edge with fewer than `limit` results.
        Ok((results, None))
    }

    /// The hash kinds' traversal: an **ordinal** walk over [`Table::tab2list`] order.
    ///
    /// `start` is [`Cursor::Begin`] for a `/3` call (which begins at the first object)
    /// or [`Cursor::Position(n)`](Cursor::Position) for a resume.
    ///
    /// `reverse` is deliberately **not** a parameter here: the hash kinds' `last`/`prev`
    /// *are* their `first`/`next`, so their reverse traversal is their forward
    /// traversal and the carried direction degenerates (see the module docs). A
    /// [`Cursor::Anchor`] cannot reach a hash table for the same reason
    /// [`Self::traverse_ordered`] documents.
    ///
    /// The chunking rule is [`Self::traverse_ordered`]'s: a continuation exactly when
    /// `limit` results were produced, carrying the ordinal just past the object that
    /// produced the `limit`-th one.
    fn traverse_positional(
        &self,
        spec: &MatchSpec,
        limit: usize,
        start: &Cursor,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<Cursor>)> {
        let objects = self.tab2list();
        let mut index = match start {
            Cursor::Position(position) => *position,
            Cursor::Begin | Cursor::Anchor(_) => 0,
        };
        let mut results = Vec::new();
        while index < objects.len() && results.len() < limit {
            if let Some(value) = run_clauses(spec, objects[index], atoms, heap)? {
                results.push(value.into_term(atoms)?);
            }
            index += 1;
        }
        // The oracle returns a continuation whenever it produced exactly `limit`
        // results — even if the limit landed on the table's last object — so the
        // resume position is the index we stopped at (which may equal the end).
        if results.len() >= limit {
            Ok((results, Some(Cursor::Position(index))))
        } else {
            Ok((results, None))
        }
    }
    /// The identity a continuation issued right now would carry for this table: the
    /// name it is registered under for a **`named_table`** (`None` otherwise). The
    /// oracle's continuation stores the name atom for a named table, so comparing it
    /// at every resume reproduces the oracle's `badarg` for a continuation resumed
    /// after an `ets:rename/2`.
    fn continuation_name(&self) -> Option<Atom> {
        if self.is_named_table() {
            self.name()
        } else {
            None
        }
    }

    /// Resume a continuation: recheck the table identity, rerun the carried spec
    /// from the carried cursor with the carried limit and direction, and rebuild the
    /// continuation when another full chunk is produced.
    ///
    /// The identity check runs **before** the cursor is consumed, so a foreign or
    /// stale continuation never reaches a traversal (the read gate / `ETS_SURFACE.md`
    /// §7.1).
    ///
    /// The identity check is the crate's model of the oracle's "the table
    /// identifier does not resolve" `badarg`: a continuation from another table, or
    /// one issued before a `named_table` was renamed away from the name it was
    /// issued under, is rejected. Note that the entry point's *name* is deliberately
    /// irrelevant — [`Table::select_continue`], [`Table::select_reverse_continue`],
    /// [`Table::match_continue`] and [`Table::match_object_continue`] are the same
    /// resumption, exactly as in the oracle.
    fn resume(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        if cont.table_id != self.id() || cont.name != self.continuation_name() {
            return Err(badarg());
        }
        let (results, resume) = self.traverse(
            &cont.spec,
            cont.limit,
            &cont.cursor,
            cont.reverse,
            atoms,
            heap,
        )?;
        Ok((results, resume.map(|cursor| cont.at(cursor))))
    }

    /// `ets:select/3` — scan at most `limit` results, returning a continuation
    /// when the limit is reached and `None` when the traversal finished.
    ///
    /// `limit == 0` is `badarg` (the oracle raises `error:badarg`).
    pub fn select_limit(
        &self,
        spec: &MatchSpec,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        if limit == 0 {
            return Err(badarg());
        }
        let (results, resume) = self.traverse(spec, limit, &Cursor::Begin, false, atoms, heap)?;
        let continuation = resume.map(|cursor| MatchContinuation {
            table_id: self.id(),
            name: self.continuation_name(),
            spec: spec.clone(),
            limit,
            cursor,
            reverse: false,
        });
        Ok((results, continuation))
    }

    /// `ets:select/1` — resume an `ets:select/3` traversal.
    pub fn select_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.resume(cont, atoms, heap)
    }

    /// `ets:select_reverse/3` — the reverse-ordered analogue of
    /// [`Self::select_limit`]: walk the table in the reverse of [`Table::tab2list`]
    /// order (descending key order for an `ordered_set`), producing at most `limit`
    /// results and a continuation when the limit is reached.
    ///
    /// The chunking rules are exactly [`Self::select_limit`]'s (`limit == 0` ⇒
    /// `badarg`; a full chunk — even one landing on the last object ⇒ a
    /// continuation; a partial final chunk ⇒ `None`). The returned continuation
    /// carries the reverse direction, so its [`Self::select_reverse_continue`]
    /// resumptions stay in reverse order.
    ///
    /// For the **hash kinds the direction is a no-op**: ERTS's `last`/`prev` *are* its
    /// `first`/`next`, so their reverse traversal is their forward traversal (see the
    /// module docs), and this form yields exactly what [`Self::select_limit`] yields,
    /// chunk for chunk.
    ///
    /// Verified against the system `erl` (OTP 29 / erts-17.1): on the `ordered_set`
    /// `{1,a}..{5,e}` with `limit = 2` the chunks are `[{5,e},{4,d}]`+cont ·
    /// `[{3,c},{2,b}]`+cont · `([{1,a}], None)`.
    pub fn select_reverse_limit(
        &self,
        spec: &MatchSpec,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        if limit == 0 {
            return Err(badarg());
        }
        let (results, resume) = self.traverse(spec, limit, &Cursor::Begin, true, atoms, heap)?;
        let continuation = resume.map(|cursor| MatchContinuation {
            table_id: self.id(),
            name: self.continuation_name(),
            spec: spec.clone(),
            limit,
            cursor,
            reverse: true,
        });
        Ok((results, continuation))
    }
    /// `ets:select_reverse/1` — resume an `ets:select_reverse/3` traversal,
    /// continuing in the same reverse order.
    pub fn select_reverse_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.resume(cont, atoms, heap)
    }

    /// `ets:match_object/3` — the whole object for every match, limited.
    ///
    /// Builds the `[{'$_' body}]` spec from the pattern (exactly like
    /// [`Table::match_object`]) and then behaves like [`Table::select_limit`].
    pub fn match_object_limit(
        &self,
        pattern_term: Eterm,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        let spec = spec_for_pattern(pattern_term, atoms, BodyKind::WholeObject)?;
        self.select_limit(&spec, limit, atoms, heap)
    }

    /// `ets:match_object/1` — resume an `ets:match_object/3` traversal.
    pub fn match_object_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.resume(cont, atoms, heap)
    }

    /// `ets:match/3` — the bound variables per match, limited.
    ///
    /// Builds the `[{'$$' body}]` spec from the pattern (exactly like
    /// [`Table::match_`]) and then behaves like [`Table::select_limit`].
    pub fn match_limit(
        &self,
        pattern_term: Eterm,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        let spec = spec_for_pattern(pattern_term, atoms, BodyKind::AllBindings)?;
        self.select_limit(&spec, limit, atoms, heap)
    }

    /// `ets:match/1` — resume an `ets:match/3` traversal.
    pub fn match_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.resume(cont, atoms, heap)
    }
}

impl MatchContinuation {
    /// A copy of `self` resuming at `cursor` (same table identity, spec, limit and
    /// traversal direction).
    fn at(&self, cursor: Cursor) -> MatchContinuation {
        MatchContinuation {
            table_id: self.table_id,
            name: self.name,
            spec: self.spec.clone(),
            limit: self.limit,
            cursor,
            reverse: self.reverse,
        }
    }
}