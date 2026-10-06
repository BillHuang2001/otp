//! Limit + continuation forms of the select family: `ets:select/3`+`select/1`,
//! `ets:match/3`+`match/1` and `ets:match_object/3`+`match_object/1`.
//!
//! A `{Results, Continuation}` pair is returned when the limit is reached; when
//! the traversal finishes, `Option::None` stands for the BIF's `'$end_of_table'`
//! (rendered as the *bare* atom when no results were produced — see the crate
//! `CONTEXT.md`).
//!
//! (Skeleton: the bodies are filled in by the continuation task.)

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::MatchSpec;
use crate::table::Table;

/// An opaque, resumable traversal position for the `ets:select/3` family.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchContinuation {
    /// The limit the originating `/3` call used (reused by `/1`).
    pub limit: usize,
}

impl Table {
    /// `ets:select/3` — scan at most `limit` results, returning a continuation
    /// when the limit is reached and `None` when the traversal finished.
    pub fn select_limit(
        &self,
        _spec: &MatchSpec,
        _limit: usize,
        _atoms: &AtomTable,
        _heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        Err(crate::error::unsupported("select_limit: not yet implemented"))
    }

    /// `ets:select/1` — resume a `ets:select/3` traversal.
    pub fn select_continue(
        &self,
        _cont: &MatchContinuation,
        _atoms: &AtomTable,
        _heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        Err(crate::error::unsupported("select_continue: not yet implemented"))
    }

    /// `ets:match_object/3` — the whole object for every match, limited.
    pub fn match_object_limit(
        &self,
        _pattern_term: Eterm,
        _limit: usize,
        _atoms: &AtomTable,
        _heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        Err(crate::error::unsupported("match_object_limit: not yet implemented"))
    }

    /// `ets:match_object/1` — resume an `ets:match_object/3` traversal.
    pub fn match_object_continue(
        &self,
        _cont: &MatchContinuation,
        _atoms: &AtomTable,
        _heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        Err(crate::error::unsupported("match_object_continue: not yet implemented"))
    }

    /// `ets:match/3` — the bound variables per match, limited.
    pub fn match_limit(
        &self,
        _pattern_term: Eterm,
        _limit: usize,
        _atoms: &AtomTable,
        _heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        Err(crate::error::unsupported("match_limit: not yet implemented"))
    }

    /// `ets:match/1` — resume an `ets:match/3` traversal.
    pub fn match_continue(
        &self,
        _cont: &MatchContinuation,
        _atoms: &AtomTable,
        _heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        Err(crate::error::unsupported("match_continue: not yet implemented"))
    }
}
