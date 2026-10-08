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
//! `ets:match/3`+`match/1` and `ets:match_object/3`+`match_object/1`.
//!
//! # What these forms do
//!
//! Given a compiled match spec and a `limit`, the `/3` forms scan the table's
//! objects in [`Table::tab2list`] order, apply the spec to each (via the same
//! [`super::exec::run_clauses`] the non-limited forms use) and collect the body
//! values. As soon as `limit` results have been produced the scan **stops** and a
//! [`MatchContinuation`] is returned that can resume where it left off. When the
//! scan reaches the end of the table with fewer than `limit` results, no
//! continuation is produced.
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
//! * Resuming a continuation whose stored position is already at/after the end ⇒
//!   `([], None)`.
//! * [`Table::select_continue`]/[`Table::match_continue`]/
//!   [`Table::match_object_continue`] resume with the **same** `limit` the
//!   originating `/3` used (the continuation carries it), so a `limit`-sized
//!   chunk is reproduced exactly.
//!
//! Concretely, on a 5-object `ordered_set` `{1,a}`…`{5,e}` with the spec
//! `[{'$1',[],['$1']}]` and `limit = 2` the chunks are
//! `[{1,a},{2,b}]`+cont · `[{3,c},{4,d}]`+cont · `([{5,e}], None)`; on a 4-object
//! table limit 2 gives `2`+cont · `2`+cont · `([], None)` (the oracle renders that
//! last one as the *bare* atom `'$end_of_table'`).
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
//! # Continuations are index positions, not live cursors
//!
//! A continuation stores the scan position as an **index into the table's
//! iteration order**. This is faithful for an unmodified table (the tests hold the
//! table fixed between chunks). A table mutated between a `/3` call and its `/1`
//! resume may shift that index (the oracle instead keeps an internal cursor); the
//! position is still bounds-checked, so a stale index never reads out of bounds —
//! it just resumes at a possibly different object. This is the same "hash-kind
//! order is unspecified" freedom the crate documents elsewhere.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::exec::{run_clauses, spec_for_pattern, BodyKind};
use super::MatchSpec;
use crate::error::badarg;
use crate::table::{Table, TableId};

/// An opaque, resumable traversal position for the `ets:select/3` family.
///
/// Constructed only by the `/3` methods; its fields are private so a caller can
/// never fabricate one. It carries the identity of the table it belongs to, the
/// compiled spec it is running, the limit to reuse on resume, and the index into
/// the table's iteration order at which the next resume continues.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchContinuation {
    /// The id of the table the traversal belongs to (`Table::id`). A continuation
    /// offered to a *different* table is rejected with `badarg` rather than being
    /// applied to unrelated objects.
    table_id: TableId,
    /// The compiled spec the traversal runs (cloned from the originating `/3`
    /// call, so a `/1` resume needs no spec argument).
    spec: MatchSpec,
    /// The limit the originating `/3` call used (reused by `/1`).
    limit: usize,
    /// The index into the table's `tab2list()` order to resume at.
    position: usize,
}

impl Table {
    /// The shared traversal both the `/3` forms and their `/1` resumptions run:
    /// scan from `start`, produce at most `limit` results, and report either the
    /// index to resume at (the oracle returns a continuation whenever exactly
    /// `limit` results were produced) or `None` when the scan reached the end.
    ///
    /// `limit` must be non-zero (the `/3` entry points reject `0` with `badarg`);
    /// the `/1` entry points always carry a limit their originating `/3` already
    /// validated.
    fn traverse(
        &self,
        spec: &MatchSpec,
        limit: usize,
        start: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<usize>)> {
        let objects = self.tab2list();
        let mut results = Vec::new();
        let mut index = start;
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
            Ok((results, Some(index)))
        } else {
            Ok((results, None))
        }
    }

    /// Resume a continuation: recheck the table, rerun the carried spec from the
    /// carried position with the carried limit, and rebuild the continuation when
    /// another full chunk is produced.
    fn resume(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        if cont.table_id != self.id() {
            return Err(badarg());
        }
        let (results, resume) =
            self.traverse(&cont.spec, cont.limit, cont.position, atoms, heap)?;
        Ok((results, resume.map(|position| cont.at(position))))
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
        let (results, resume) = self.traverse(spec, limit, 0, atoms, heap)?;
        let continuation = resume.map(|position| MatchContinuation {
            table_id: self.id(),
            spec: spec.clone(),
            limit,
            position,
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
    /// A copy of `self` resuming at `position` (same table, spec and limit).
    fn at(&self, position: usize) -> MatchContinuation {
        MatchContinuation {
            table_id: self.table_id,
            spec: self.spec.clone(),
            limit: self.limit,
            position,
        }
    }
}
