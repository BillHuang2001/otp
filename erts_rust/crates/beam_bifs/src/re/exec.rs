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

//! The matcher: an iterative backtracking VM over the compiled [`Program`],
//! the single-shot search, and the oracle's `global` loop.
//!
//! # The VM
//!
//! The VM keeps its **entire** state in explicit data structures — a
//! program counter, a subject position, the capture slots, a choice-point
//! stack, an undo log and a mark stack — and never recurses (neither for a
//! quantifier nor for the search). A 20 000-codepoint subject therefore cannot
//! overflow the Rust stack (pinned by the tests).
//!
//! * `Split { a, b }` pushes a choice point for its **second** arm and
//!   continues at its first, which is what makes the greedy/lazy ordering fall
//!   out of the code generator's arm placement.
//! * A choice point records the length of the undo log (`saves`) and the mark
//!   depth (`marks`) at the moment it was taken, so backtracking is: pop the
//!   choice point, replay the log in reverse down to `saves`, truncate the
//!   stacks and restore `pc`/`pos`.
//! * `Save(slot)` / `Match` log an `Undo::Restore` for every slot they write,
//!   so a capture is properly unwound when the branch that set it is abandoned.
//! * `Mark`/`Cut` (an atomic group, or a possessive quantifier) push/pop the
//!   choice-point depth; a `Cut` **discards** the choice points above its mark,
//!   which is exactly the commit an atomic group asks for. Undoing a `Cut`
//!   restores the mark, but not the discarded choice points — they are gone for
//!   good, as PCRE2's atomicity requires.
//!
//! # The empty-loop guard (a documented approximation)
//!
//! A loop whose body can match the empty string (`` (a?)* ``, `` (?:)* ``)
//! would otherwise iterate forever. A `Split` is treated as a *loop head* when
//! one of its arms is its immediate successor `pc + 1` (the code generator
//! always lays a repeated body out directly after the `Split`, greedy or lazy),
//! and a `HashMap<u32, usize>` remembers the subject position at which that
//! head's body arm was last entered. When the same head is reached again at the
//! same position the body arm is **not** re-entered: the engine takes the exit
//! arm instead, so an iteration that consumes nothing is not repeated.
//!
//! Two honest caveats: (a) an alternation's `Split` also has an arm equal to
//! `pc + 1`, so it is classified as a loop head too — harmless, because a
//! `Split` is only ever *re-dispatched* at the same position through a backward
//! `Jmp`, and every backward jump the code generator emits targets a real loop
//! head; and (b) the map is **not** undone when the VM backtracks (only at the
//! start of each attempt), so a position recorded on a branch that is later
//! abandoned can suppress a re-entry. That is the price of guaranteed
//! termination; the alternative is an explicit iteration counter.
//!
//! # The step budget (`match_limit`)
//!
//! `steps` counts dispatched instructions and is compared against
//! `{match_limit, N}` (default [`DEFAULT_MATCH_LIMIT`]) once per dispatched
//! instruction; the attempt then ends in [`Outcome::MatchLimit`], which is
//! propagated out of the whole search rather than being folded into "no match".
//! PCRE2 counts internal `match()` calls, so for a *small* `N` the threshold at
//! which this engine gives up differs from PCRE2's; `{match_limit, 0}` is
//! oracle-exact (`re:run("", "", [{match_limit,0}, report_errors])` is
//! `{error, match_limit}`), because a budget of zero exhausts on the very first
//! instruction. `steps` is reset by [`Machine::single_shot`], i.e. per
//! `pcre2_match`-equivalent call — which is also the granularity of the
//! oracle's budget, since its `global` loop (`re:grun`) makes one such call per
//! iteration.
//!
//! # The assertions
//!
//! `^ $ \A \z \Z \b \B` follow [`Assert`]: the multiline/dollar-endonly flags
//! and the newline convention are baked into the instruction, except that a
//! run-time `{newline, _}` (which a caller may pass to `re:run/3` on an
//! *already compiled* pattern) overrides the compiled one. `\G` ([`Assert::
//! StartMatch`]) compares against the **current call's** search offset, which is
//! what `PCRE2`'s `startoffset` means — oracle-verified:
//! `re:run("abcabc", "\\G.", [{offset,2},global])` is
//! `{match,[[{2,1}],[{3,1}],[{4,1}],[{5,1}]]}` (the offset advances every
//! iteration), not the single `{2,1}` a fixed offset would produce.
//!
//! # The `global` loop
//!
//! [`Machine::run_global`] is `lib/stdlib/src/re.erl`'s `loopexec/8` (lines
//! 1841-1888) *flattened* into a loop: `loopexec` is a linear (non-branching)
//! recursion, so the piecewise concatenation it builds is accumulated in order.
//! Two details of that function that a naive reading misses are reproduced
//! deliberately:
//!
//! * The `B > 0` test is on the **length** of the whole match (`{Start,Len}` as
//!   `re:index()` returns it), not on its end offset — an empty match at
//!   offset 3 has `B = 0` even though its end is 3.
//! * `loopexec` drops a **leading duplicate** of the match it just reported
//!   from the recursive result (`Rest1` in the source). Without it,
//!   `re:run(" ab", "\\b", [global])` would report `{1,0}` twice. The flattened
//!   loop models that with a one-element lookahead ("suppress the next reported
//!   match if it equals this one").

use std::collections::HashMap;

use crate::error::badarg;
use crate::re::options::{Newline, Parsed};
use crate::re::prog::{Inst, Program};
use crate::re::syntax::{chars_equal, ranges_contain, Assert};

/// The instruction budget of one `pcre2_match`-equivalent call when the caller
/// gives no `{match_limit, N}` (`PCRE2`'s own default is 10 000 000).
const DEFAULT_MATCH_LIMIT: u64 = 10_000_000;

// ---------------------------------------------------------------------------
// The subject
// ---------------------------------------------------------------------------

/// A flattened subject: one codepoint per `codes` entry, with the byte offset of
/// each codepoint (so a match can be rendered as `{Start, Len}` byte offsets, as
/// the oracle's `index` capture type does) and the original bytes.
///
/// `offs` is `codes.len() + 1` long: `offs[i]` is the byte offset of codepoint
/// `i` and `offs[codes.len()] == bytes.len()`, so the byte span of a codepoint
/// range is `(offs[a], offs[b])`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Subject {
    /// The codepoints.
    pub codes: Vec<u32>,
    /// The byte offset of each codepoint, plus the total byte length.
    pub offs: Vec<usize>,
    /// The subject's bytes.
    pub bytes: Vec<u8>,
}

impl Subject {
    /// A subject whose bytes are its codepoints, one per byte (the oracle's
    /// non-`unicode` mode).
    pub(crate) fn latin1(bytes: Vec<u8>) -> Subject {
        let codes: Vec<u32> = bytes.iter().map(|&b| u32::from(b)).collect();
        let offs: Vec<usize> = (0..=bytes.len()).collect();
        Subject { codes, offs, bytes }
    }

    /// A subject decoded as UTF-8 (the oracle's `unicode` mode).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the bytes are
    /// not valid UTF-8 — the oracle refuses such a subject with `badarg` (there
    /// is no "match the raw bytes" fallback in `unicode` mode). Note that a
    /// surrogate encoded in UTF-8 (CESU-8) and an overlong encoding are invalid
    /// UTF-8 here, as they are for PCRE2.
    pub(crate) fn utf8(bytes: Vec<u8>) -> beam_common::Result<Subject> {
        let text = std::str::from_utf8(&bytes).map_err(|_| badarg())?;
        let mut codes: Vec<u32> = Vec::with_capacity(text.chars().count());
        let mut offs: Vec<usize> = Vec::with_capacity(codes.capacity() + 1);
        for (offset, ch) in text.char_indices() {
            offs.push(offset);
            codes.push(ch as u32);
        }
        offs.push(bytes.len());
        Ok(Subject { codes, offs, bytes })
    }

    /// The byte span `{Start, Len}`-style `(start, end)` pair of a codepoint
    /// span. A span outside the subject is clamped, never a panic.
    pub(crate) fn byte_span(&self, span: (usize, usize)) -> (usize, usize) {
        let (start, end) = self.clamp(span);
        (self.offs[start], self.offs[end])
    }

    /// The codepoints of a span. A span outside the subject is clamped.
    pub(crate) fn codepoints(&self, span: (usize, usize)) -> &[u32] {
        let (start, end) = self.clamp(span);
        &self.codes[start..end]
    }

    /// A span clamped to the subject, with `start <= end`.
    fn clamp(&self, span: (usize, usize)) -> (usize, usize) {
        let n = self.codes.len();
        let start = span.0.min(n);
        let end = span.1.clamp(start, n);
        (start, end)
    }
}

// ---------------------------------------------------------------------------
// The result
// ---------------------------------------------------------------------------

/// The capture slots of one match, as **codepoint** spans.
///
/// `slots` is `2 * (ncap + 1)` long: `slots[0]`/`slots[1]` are the whole match
/// and `slots[2*g]`/`slots[2*g + 1]` are group `g` (so the emitter's
/// `Save(2*idx)`/`Save(2*idx + 1)` write group `idx`). `None` is a group that
/// did not participate.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Captures {
    /// The slots.
    pub slots: Vec<Option<(usize, usize)>>,
}

impl Captures {
    /// The span of group `g` (group `0` is the whole match), or `None` when the
    /// group did not participate (or does not exist in this pattern).
    pub(crate) fn group(&self, g: u32) -> Option<(usize, usize)> {
        group_span(&self.slots, g)
    }
}

/// The span of group `g` in a slot vector — the shared accessor of
/// [`Captures::group`] and the VM.
///
/// The **end** slot (`2 * g + 1`) is the one carrying the completed span: the
/// even slot only holds the group's start position, written as the degenerate
/// `(pos, pos)` marker when the group opens (`Save(2 * g)`), while the odd one
/// is written as `(start, pos)` when it closes (`Save(2 * g + 1)`). Group 0 is
/// the exception only in that `Match` writes the whole-match span into *both* of
/// its slots, so reading the odd slot is right for every group.
fn group_span(slots: &[Option<(usize, usize)>], g: u32) -> Option<(usize, usize)> {
    let idx = (g as usize).checked_mul(2)?.checked_add(1)?;
    slots.get(idx).copied().flatten()
}

/// The position at which group `g` opened — the even slot's value, or `None`
/// when the group has not been entered (or does not exist).
fn group_start(slots: &[Option<(usize, usize)>], g: u32) -> Option<usize> {
    let idx = (g as usize).checked_mul(2)?;
    slots.get(idx).copied().flatten().map(|(start, _)| start)
}

/// What [`run`] found: the matches (one element per reported match; a
/// non-`global` call reports at most one), no match at all, or an exhausted
/// match limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The reported matches, in subject order (`global`), or the single match.
    Match(Vec<Captures>),
    /// Nothing matched — the oracle's bare `nomatch`.
    NoMatch,
    /// The `{match_limit, N}` budget was exhausted.
    MatchLimit,
}

/// Matches `program` against `subject` under the run options `parsed`.
///
/// `step_ids` is the set of groups whose *rendered* values the caller will
/// return. It is used by the `global` loop to detect the duplicate matches
/// `loopexec` drops, because the oracle compares the terms it would return.
///
/// Never fails: a pattern/option combination the engine cannot execute never
/// reaches this function (it is a typed capability gap raised by the caller),
/// and the VM itself treats a malformed program (an out-of-range `Class` index
/// or jump target) as "this branch fails" rather than as an error, so it can
/// never produce a wrong match.
pub(crate) fn run(
    program: &Program,
    subject: &Subject,
    parsed: &Parsed,
    step_ids: &[u32],
) -> Outcome {
    // A run-time `{newline, _}` overrides the convention the pattern was
    // compiled with (`re:run/3` may pass one for an already compiled pattern).
    let nl = parsed.newline_opt.unwrap_or(program.flags.newline);
    let mut machine = Machine::new(program, &subject.codes, parsed, nl);
    if parsed.global {
        machine.run_global(step_ids)
    } else {
        match machine.single_shot(parsed.start_offset as usize, false, false) {
            Attempt::Matched(caps) => Outcome::Match(vec![caps]),
            Attempt::Failed => Outcome::NoMatch,
            Attempt::Limited => Outcome::MatchLimit,
        }
    }
}

// ---------------------------------------------------------------------------
// The VM
// ---------------------------------------------------------------------------

/// The result of one attempt (`pcre2_match` at a fixed start position).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Attempt {
    /// The attempt matched; the slots are the capture state at `Match`.
    Matched(Captures),
    /// The attempt failed and its choice points are exhausted.
    Failed,
    /// The step budget ran out.
    Limited,
}

/// What a dispatched instruction asks the driver loop to do next.
enum Flow {
    /// Continue at the (already updated) `pc`.
    Next,
    /// This instruction failed: backtrack.
    Backtrack,
    /// `Match` accepted.
    Accept,
}

/// One alternative to try when the current path fails.
#[derive(Clone, Copy, Debug)]
struct Choice {
    /// The program counter to resume at.
    pc: u32,
    /// The subject position to resume at.
    pos: usize,
    /// The undo-log length when the choice point was taken.
    saves: usize,
    /// The mark-stack depth when the choice point was taken.
    marks: usize,
}

/// The inverse of one state change, applied in reverse on backtracking.
#[derive(Clone, Copy, Debug)]
enum Undo {
    /// `slots[slot]` was `Option`; put it back.
    Restore(usize, Option<(usize, usize)>),
    /// The inverse of `Mark`: pop a mark.
    MarksPopped,
    /// The inverse of `Cut`: push the mark it popped back.
    MarksPushed(usize),
}

/// The backtracking VM — one instance per [`run`] call, reused across attempts
/// (the vectors keep their capacity, so an unanchored scan does not allocate per
/// attempt).
struct Machine<'a> {
    program: &'a Program,
    codes: &'a [u32],
    parsed: &'a Parsed,
    /// The effective newline convention (the run-time override, or the
    /// compiled one).
    nl: Newline,
    /// The instruction budget of one `single_shot` call.
    budget: u64,
    /// The instructions dispatched in the current `single_shot` call.
    steps: u64,
    /// The search offset of the current `single_shot` call — what `\G` and
    /// `notempty_atstart` compare against.
    search_offset: usize,
    /// Whether an empty match at the attempt start must be refused.
    notempty_atstart: bool,
    /// The start of the attempt being executed.
    attempt_start: usize,
    /// The program counter.
    pc: usize,
    /// The subject position (a codepoint index).
    pos: usize,
    /// The capture slots.
    slots: Vec<Option<(usize, usize)>>,
    /// The choice points.
    choices: Vec<Choice>,
    /// The undo log.
    log: Vec<Undo>,
    /// The mark stack (`Mark`/`Cut`).
    marks: Vec<usize>,
    /// Loop heads seen at a position (the empty-loop guard; see the module
    /// docs).
    loop_head_pos: HashMap<u32, usize>,
}

impl<'a> Machine<'a> {
    fn new(program: &'a Program, codes: &'a [u32], parsed: &'a Parsed, nl: Newline) -> Machine<'a> {
        let budget = u64::from(parsed.match_limit.unwrap_or(DEFAULT_MATCH_LIMIT as u32));
        Machine {
            program,
            codes,
            parsed,
            nl,
            budget,
            steps: 0,
            search_offset: parsed.start_offset as usize,
            notempty_atstart: parsed.notempty_atstart,
            attempt_start: 0,
            pc: 0,
            pos: 0,
            slots: Vec::new(),
            choices: Vec::new(),
            log: Vec::new(),
            marks: Vec::new(),
            loop_head_pos: HashMap::new(),
        }
    }

    /// Resets the per-attempt state.
    fn begin(&mut self, attempt_start: usize) {
        self.pc = 0;
        self.pos = attempt_start;
        self.attempt_start = attempt_start;
        self.choices.clear();
        self.log.clear();
        self.marks.clear();
        self.loop_head_pos.clear();
        self.slots.clear();
        self.slots.resize(2 * (self.program.ncap as usize + 1), None);
    }

    // -- the search ---------------------------------------------------------

    /// The single-shot search: the oracle's one `re:internal_run/4` call.
    ///
    /// Tries `attempt_start = search_offset, search_offset + 1, …` (up to the
    /// `firstline` bound, and only the search offset itself when the match is
    /// anchored) and returns the first attempt that matches.
    fn single_shot(
        &mut self,
        search_offset: usize,
        force_anchored: bool,
        force_notempty_atstart: bool,
    ) -> Attempt {
        let len = self.codes.len();
        if search_offset > len {
            return Attempt::Failed;
        }
        self.search_offset = search_offset;
        self.notempty_atstart = self.parsed.notempty_atstart || force_notempty_atstart;
        self.steps = 0;
        let anchored =
            force_anchored || self.parsed.compile.anchored || self.program.flags.anchored;
        let scan_max = if anchored {
            search_offset
        } else if self.program.flags.firstline {
            // `PCRE2_FIRSTLINE`: the match must start at or before the first
            // newline of the subject.
            let mut m = search_offset;
            while m < len && !self.nl.is_newline_char(self.codes[m]) {
                m += 1;
            }
            m
        } else {
            len
        };
        for attempt_start in search_offset..=scan_max {
            match self.attempt(attempt_start) {
                Attempt::Failed => {}
                found => return found,
            }
        }
        Attempt::Failed
    }

    /// One attempt at a fixed start position (`pcre2_match` with
    /// `startoffset = attempt_start` and no bump-along).
    fn attempt(&mut self, attempt_start: usize) -> Attempt {
        self.begin(attempt_start);
        loop {
            self.steps += 1;
            if self.steps > self.budget {
                return Attempt::Limited;
            }
            let flow = match self.program.insts.get(self.pc) {
                Some(&inst) => self.dispatch(inst),
                // A jump target or class index outside the program: this branch
                // simply fails. A program this engine compiled cannot contain
                // one, and a decoded payload is validated by `prog::Payload`.
                None => Flow::Backtrack,
            };
            match flow {
                Flow::Next => {}
                Flow::Backtrack => {
                    if !self.backtrack() {
                        return Attempt::Failed;
                    }
                }
                Flow::Accept => return Attempt::Matched(Captures { slots: self.slots.clone() }),
            }
        }
    }

    /// One dispatched instruction. `pc`/`pos` are advanced by the arm, so the
    /// caller only has to act on the returned [`Flow`].
    fn dispatch(&mut self, inst: Inst) -> Flow {
        match inst {
            Inst::Char { c, caseless } => match self.codes.get(self.pos) {
                Some(&ch) if chars_equal(ch, c, caseless) => {
                    self.pos += 1;
                    self.pc += 1;
                    Flow::Next
                }
                _ => Flow::Backtrack,
            },
            Inst::Class(id) => {
                let hit = self.program.classes.get(id as usize).is_some_and(|ranges| {
                    self.codes.get(self.pos).is_some_and(|&ch| ranges_contain(ranges, ch))
                });
                self.advance_if(hit)
            }
            Inst::Any { dotall } => {
                let hit = match self.codes.get(self.pos) {
                    Some(&ch) => dotall || !self.nl.is_newline_char(ch),
                    None => false,
                };
                self.advance_if(hit)
            }
            Inst::AnyButNewline => {
                let hit = match self.codes.get(self.pos) {
                    Some(&ch) => !self.nl.is_newline_char(ch),
                    None => false,
                };
                self.advance_if(hit)
            }
            Inst::Assert(assert) => {
                if self.assert_holds(assert) {
                    self.pc += 1;
                    Flow::Next
                } else {
                    Flow::Backtrack
                }
            }
            Inst::Split { a, b } => self.split(a as usize, b as usize),
            Inst::Jmp { target } => {
                self.pc = target as usize;
                Flow::Next
            }
            Inst::Save(slot) => {
                self.save(slot as usize);
                self.pc += 1;
                Flow::Next
            }
            Inst::BackRef { group, caseless } => self.backref(group, caseless),
            Inst::Mark => {
                self.marks.push(self.choices.len());
                self.log.push(Undo::MarksPopped);
                self.pc += 1;
                Flow::Next
            }
            Inst::Cut => {
                if let Some(mark) = self.marks.pop() {
                    // An atomic group commits: everything the enclosed construct
                    // put on the choice stack is discarded for good.
                    self.choices.truncate(mark);
                    self.log.push(Undo::MarksPushed(mark));
                }
                self.pc += 1;
                Flow::Next
            }
            Inst::Match => {
                self.set_slot(0, Some((self.attempt_start, self.pos)));
                self.set_slot(1, Some((self.attempt_start, self.pos)));
                if self.empty_match_refused() {
                    Flow::Backtrack
                } else {
                    Flow::Accept
                }
            }
            Inst::Fail => Flow::Backtrack,
        }
    }

    /// `Split { a, b }`: push a choice point for `b` and continue at `a`, with
    /// the empty-loop guard (see the module docs).
    fn split(&mut self, a: usize, b: usize) -> Flow {
        let head = if a == self.pc + 1 {
            Some(b)
        } else if b == self.pc + 1 {
            Some(a)
        } else {
            None
        };
        if let Some(exit) = head {
            if self.loop_head_pos.get(&(self.pc as u32)) == Some(&self.pos) {
                // The body arm was already entered at this position: an
                // iteration that consumed nothing is not repeated.
                self.pc = exit;
                return Flow::Next;
            }
            self.loop_head_pos.insert(self.pc as u32, self.pos);
        }
        self.choices.push(Choice {
            pc: b as u32,
            pos: self.pos,
            saves: self.log.len(),
            marks: self.marks.len(),
        });
        self.pc = a;
        Flow::Next
    }

    /// `Save(slot)` — the even slot starts a group, the odd one ends it.
    fn save(&mut self, slot: usize) {
        if slot % 2 == 0 {
            self.set_slot(slot, Some((self.pos, self.pos)));
        } else {
            // The group's start marker; a missing one (an odd slot with no even
            // predecessor in the program) falls back to the current position.
            let start = group_start(&self.slots, ((slot - 1) / 2) as u32).unwrap_or(self.pos);
            self.set_slot(slot, Some((start, self.pos)));
        }
    }

    /// Writes a capture slot, logging its old value so backtracking can undo
    /// the write. A slot outside the vector is ignored.
    fn set_slot(&mut self, slot: usize, value: Option<(usize, usize)>) {
        let Some(old) = self.slots.get(slot).copied() else {
            return;
        };
        self.log.push(Undo::Restore(slot, old));
        self.slots[slot] = value;
    }

    /// `BackRef { group }` — match the text the group captured. An unset group
    /// makes the backreference fail (PCRE2's behaviour for a group that never
    /// participated).
    fn backref(&mut self, group: u32, caseless: bool) -> Flow {
        let Some((a, b)) = group_span(&self.slots, group) else {
            return Flow::Backtrack;
        };
        let n = match b.checked_sub(a) {
            Some(n) => n,
            None => return Flow::Backtrack,
        };
        if self.pos + n > self.codes.len() {
            return Flow::Backtrack;
        }
        let hit = (0..n)
            .all(|k| chars_equal(self.codes[self.pos + k], self.codes[a + k], caseless));
        self.advance_if(hit)
    }

    /// `pos += 1; pc += 1` when `cond`, else fail.
    fn advance_if(&mut self, cond: bool) -> Flow {
        if cond {
            self.pos += 1;
            self.pc += 1;
            Flow::Next
        } else {
            Flow::Backtrack
        }
    }

    /// Pops a choice point and rewinds the log/stacks to the state it captured.
    /// Returns `false` when there is nothing left to try.
    fn backtrack(&mut self) -> bool {
        let Some(choice) = self.choices.pop() else {
            return false;
        };
        while self.log.len() > choice.saves {
            match self.log.pop() {
                Some(Undo::Restore(slot, old)) => {
                    if let Some(cell) = self.slots.get_mut(slot) {
                        *cell = old;
                    }
                }
                Some(Undo::MarksPopped) => {
                    self.marks.pop();
                }
                Some(Undo::MarksPushed(mark)) => {
                    self.marks.push(mark);
                }
                // The loop condition guarantees a `Some`.
                None => break,
            }
        }
        self.marks.truncate(choice.marks);
        self.pc = choice.pc as usize;
        self.pos = choice.pos;
        true
    }

    // -- assertions ---------------------------------------------------------

    /// Whether a zero-width assertion holds at `self.pos`.
    fn assert_holds(&self, assert: Assert) -> bool {
        let len = self.codes.len();
        let pos = self.pos;
        match assert {
            Assert::StartLine { multiline } => {
                if multiline {
                    // PCRE2's `OP_CIRC` requires `ptr < end_subject` for the
                    // after-a-newline arm, so a trailing newline does *not* open
                    // a line at the end of the subject (oracle-verified:
                    // `re:run("a\nb\n","^",[global,multiline,…])` reports only
                    // offsets 0 and 2, and `re:run("\n","^",[{offset,1},multiline,…])`
                    // is `nomatch`).
                    (pos == 0 && !self.parsed.notbol)
                        || (pos < len && pos != 0 && self.nl.ends_at(self.codes, pos))
                } else {
                    pos == 0 && !self.parsed.notbol
                }
            }
            Assert::EndLine { multiline, dollar_endonly } => {
                if multiline {
                    // `dollar_endonly` is ignored in multiline mode.
                    (pos == len && !self.parsed.noteol)
                        || self.nl.newline_len_at(self.codes, pos).is_some()
                } else {
                    !self.parsed.noteol
                        && (pos == len
                            || (!dollar_endonly
                                && self
                                    .nl
                                    .newline_len_at(self.codes, pos)
                                    .is_some_and(|n| pos + n == len)))
                }
            }
            // None of these three is affected by `notbol`/`noteol`
            // (oracle-verified: `re:run("abc","\\Aa",[notbol,…])` matches and
            // `re:run(<<"abc\n">>,"c\\Z",[noteol,…])` matches).
            Assert::StartSubject => pos == 0,
            Assert::EndSubject => pos == len,
            Assert::EndSubjectOpt => {
                pos == len
                    || self
                        .nl
                        .newline_len_at(self.codes, pos)
                        .is_some_and(|n| pos + n == len)
            }
            Assert::StartMatch => pos == self.search_offset,
            Assert::WordBoundary(want) => {
                let before = pos > 0 && is_word(self.codes[pos - 1]);
                let after = pos < len && is_word(self.codes[pos]);
                (before != after) == want
            }
        }
    }

    /// The empty-match rules of `Match`.
    fn empty_match_refused(&self) -> bool {
        self.pos == self.attempt_start
            && (self.parsed.notempty
                || (self.notempty_atstart && self.attempt_start == self.search_offset))
    }

    // -- the global loop ----------------------------------------------------

    /// The oracle's `re.erl` `loopexec/8`, flattened (see the module docs).
    fn run_global(&mut self, step_ids: &[u32]) -> Outcome {
        let len = self.codes.len();
        let mut out: Vec<Captures> = Vec::new();
        let mut x = self.parsed.start_offset as usize;
        // The `Rest1` lookahead: when the level we just finished may have
        // produced a duplicate of its own match at the head of the recursion, it
        // is suppressed here.
        let mut suppress: Option<Vec<Option<(usize, usize)>>> = None;
        loop {
            if x > len {
                break;
            }
            let caps = match self.single_shot(x, false, false) {
                Attempt::Matched(caps) => caps,
                Attempt::Failed => break,
                Attempt::Limited => return Outcome::MatchLimit,
            };
            let proj = projection(&caps, step_ids);
            if suppress.as_ref() != Some(&proj) {
                out.push(caps.clone());
            }
            suppress = None;
            let (a, b) = caps.group(0).unwrap_or((x, x));
            if b > a {
                // A non-empty match: continue just after it.
                x = b;
                continue;
            }
            // An empty match at `a`: retry at exactly `x`, anchored and
            // refusing an empty match at the start.
            let retry = match self.single_shot(x, true, true) {
                Attempt::Limited => return Outcome::MatchLimit,
                Attempt::Failed => None,
                Attempt::Matched(retry) => {
                    if projection(&retry, step_ids) == proj {
                        None
                    } else {
                        Some(retry)
                    }
                }
            };
            let new_x = match retry.as_ref().and_then(|c| c.group(0)) {
                Some((start, end)) if end > start => a + (end - start),
                _ => {
                    if x == a || a == len {
                        self.forward(a, 1)
                    } else {
                        a
                    }
                }
            };
            match retry {
                Some(retry) => out.push(retry),
                None => {
                    if new_x == a {
                        // The next level restarts at the same offset and may
                        // report this very match again; `loopexec` drops the
                        // duplicate.
                        suppress = Some(proj);
                    }
                }
            }
            x = new_x;
        }
        if out.is_empty() {
            Outcome::NoMatch
        } else {
            Outcome::Match(out)
        }
    }

    /// `re.erl`'s `forward/5`: advance `n` codepoints, except that under a
    /// CRLF-like convention a `\r\n` pair is skipped as one unit.
    fn forward(&self, a: usize, n: usize) -> usize {
        let mut pos = a;
        let mut left = n;
        while left > 0 {
            let crlf = self.nl.is_crlf_like()
                && self.codes.get(pos) == Some(&13)
                && self.codes.get(pos + 1) == Some(&10);
            pos += if crlf { 2 } else { 1 };
            left -= 1;
        }
        pos
    }
}

/// The projection of a match's captures onto the groups the caller returns —
/// what the oracle compares when it deduplicates the `global` loop's matches
/// (it compares the terms it would return, which are exactly these spans).
fn projection(caps: &Captures, step_ids: &[u32]) -> Vec<Option<(usize, usize)>> {
    step_ids.iter().map(|&g| caps.group(g)).collect()
}

/// Whether `c` is a word character — the ASCII `[0-9A-Za-z_]` set `\w` uses in
/// this engine (`\b` and `\B` are defined against it).
fn is_word(c: u32) -> bool {
    c == u32::from(b'_') || matches!(c, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::re::options::CompileFlags;
    use crate::re::{prog, syntax};

    fn compile_flags(pattern: &str, flags: &CompileFlags) -> Program {
        let codes: Vec<u32> = pattern.chars().map(|c| c as u32).collect();
        let offs: Vec<usize> = pattern.char_indices().map(|(i, _)| i).collect();
        let syntax =
            syntax::parse(&codes, &offs, pattern.len(), flags).unwrap_or_else(|e| panic!("{pattern}: {e:?}"));
        prog::compile(&syntax, flags).unwrap_or_else(|e| panic!("{pattern}: {e:?}"))
    }

    fn subject(s: &str) -> Subject {
        Subject::latin1(s.as_bytes().to_vec())
    }

    fn parsed() -> Parsed {
        Parsed::default()
    }

    /// The single match of `pattern` against `text` under `options`.
    fn matched(pattern: &str, text: &str, options: &Parsed) -> Captures {
        let program = compile_flags(pattern, &options.compile);
        match run(&program, &subject(text), options, &[0]) {
            Outcome::Match(mut caps) => {
                assert_eq!(caps.len(), 1, "{pattern} / {text}: one match");
                caps.pop().expect("one match")
            }
            other => panic!("{pattern} / {text}: {other:?}"),
        }
    }

    /// The whole-match span of the single match.
    fn span(pattern: &str, text: &str, options: &Parsed) -> (usize, usize) {
        matched(pattern, text, options).group(0).expect("group 0")
    }

    /// The whole-match spans of a `global` run.
    fn spans(pattern: &str, text: &str, options: &Parsed) -> Vec<(usize, usize)> {
        let program = compile_flags(pattern, &options.compile);
        match run(&program, &subject(text), options, &[0]) {
            Outcome::Match(caps) => {
                caps.iter().map(|c| c.group(0).expect("group 0")).collect()
            }
            other => panic!("{pattern} / {text}: {other:?}"),
        }
    }

    fn globe(pattern: &str, text: &str) -> Vec<(usize, usize)> {
        let options = Parsed { global: true, ..Parsed::default() };
        spans(pattern, text, &options)
    }

    #[test]
    fn the_capture_slots_are_the_oracle_spans() {
        // `re:run("xabcy","(b)(c)")` = {match,[{2,2},{2,1},{3,1}]}: group 0 is
        // the whole match, then the two groups in order.
        let caps = matched("(b)(c)", "xabcy", &parsed());
        assert_eq!(caps.group(0), Some((2, 4)));
        assert_eq!(caps.group(1), Some((2, 3)));
        assert_eq!(caps.group(2), Some((3, 4)));
        assert_eq!(caps.group(3), None, "there is no group 3");
        assert_eq!(caps.slots.len(), 6, "2 * (ncap + 1)");
        // A non-capturing group shifts nothing.
        let caps = matched("(?:b)(c)", "xabcy", &parsed());
        assert_eq!(caps.group(0), Some((2, 4)));
        assert_eq!(caps.group(1), Some((3, 4)));
        assert_eq!(caps.group(2), None);
    }

    #[test]
    fn greedy_lazy_and_possessive_quantifiers() {
        let one = parsed();
        assert_eq!(span("a*", "aaa", &one), (0, 3), "greedy takes everything");
        assert_eq!(span("a*?", "aaa", &one), (0, 0), "lazy prefers the empty match");
        assert_eq!(span("a*+", "aaa", &one), (0, 3), "possessive takes everything");
        // ... and a possessive star with nothing to consume still accepts the
        // empty match (the `Cut` discards the choice points, not the match).
        assert_eq!(span("a*+", "b", &one), (0, 0));
        assert_eq!(span("a{2,3}", "aaaa", &one), (0, 3), "a bounded quantifier");
        assert_eq!(span("a+?", "aaa", &one), (0, 1));
        assert_eq!(span("a?", "aaa", &one), (0, 1));
        // A possessive quantifier's `Cut` really discards the choice points: it
        // cannot give back what it consumed (oracle: `re:run("aaa","a++a")` and
        // `re:run("aaa","a*+a")` are `nomatch`, while the greedy `re:run("aaa",
        // "a*a")` matches `{0,3}`), but a following literal may still match
        // after the consumed run (`re:run("aaab","a*+b")` = `{0,4}`).
        assert_eq!(mismatch("a*+a", "aaa", &one), None);
        assert_eq!(mismatch("a++a", "aaa", &one), None);
        assert_eq!(span("a*a", "aaa", &one), (0, 3), "the greedy form backtracks");
        assert_eq!(span("a*+b", "aaab", &one), (0, 4));
    }

    #[test]
    fn a_backreference_matches_the_captured_text() {
        let one = parsed();
        assert_eq!(span("(a)\\1", "aa", &one), (0, 2));
        assert_eq!(mismatch("(a)\\1", "ab", &one), None);
        let caseless = Parsed {
            compile: CompileFlags { caseless: true, ..CompileFlags::default() },
            ..Parsed::default()
        };
        assert_eq!(span("(a)\\1", "aA", &caseless), (0, 2));
        // An unset group makes the backreference fail.
        assert_eq!(mismatch("(z)?a\\1", "a", &one), None);
    }

    /// The single match, or `None` for `NoMatch` (a `MatchLimit` panics).
    fn mismatch(pattern: &str, text: &str, options: &Parsed) -> Option<(usize, usize)> {
        let program = compile_flags(pattern, &options.compile);
        match run(&program, &subject(text), options, &[0]) {
            Outcome::Match(caps) => Some(caps[0].group(0).expect("group 0")),
            Outcome::NoMatch => None,
            Outcome::MatchLimit => panic!("{pattern} / {text}: unexpected match limit"),
        }
    }

    #[test]
    fn the_line_anchors_honour_multiline() {
        let one = parsed();
        let multiline = Parsed {
            compile: CompileFlags { multiline: true, ..CompileFlags::default() },
            ..Parsed::default()
        };
        assert_eq!(mismatch("^a", "b\na", &one), None);
        assert_eq!(span("^a", "b\na", &multiline), (2, 3));
        assert_eq!(mismatch("a$", "a\nb", &one), None);
        assert_eq!(span("a$", "a\nb", &multiline), (0, 1));
        // `$` also matches just before a trailing newline without multiline.
        assert_eq!(span("c$", "abc\n", &one), (2, 3));
        let noteol = Parsed { noteol: true, ..Parsed::default() };
        assert_eq!(mismatch("c$", "abc\n", &noteol), None);
        // `^`/`$` at the very start/end of the subject.
        assert_eq!(span("^abc$", "abc", &one), (0, 3));
        // A trailing newline does not open a line at the end of the subject:
        // PCRE2's `OP_CIRC` needs `ptr < end_subject` for the after-a-newline
        // arm (oracle: `re:run("a\nb\n","^",[global,multiline,…])` = the two
        // matches at 0 and 2 only; `re:run("\n","^",[{offset,1},multiline,…])`
        // = `nomatch`).
        let global_multiline =
            Parsed { global: true, compile: multiline.compile.clone(), ..Parsed::default() };
        assert_eq!(spans("^", "a\nb\n", &global_multiline), vec![(0, 0), (2, 2)]);
        assert_eq!(spans("^", "\n\n", &global_multiline), vec![(0, 0), (1, 1)]);
        assert_eq!(spans("^", "a\n\nb", &global_multiline), vec![(0, 0), (2, 2), (3, 3)]);
        let at_end = Parsed { start_offset: 1, ..multiline.clone() };
        assert_eq!(mismatch("^", "\n", &at_end), None, "offset 1 == len is not a line start");
    }

    #[test]
    fn the_subject_anchors_and_word_boundaries() {
        let one = parsed();
        assert_eq!(span("\\Aabc", "abc", &one), (0, 3));
        assert_eq!(span("bc\\z", "abc", &one), (1, 3));
        assert_eq!(mismatch("c\\z", "abc\n", &one), None, "\\z is the very end");
        assert_eq!(span("c\\Z", "abc\n", &one), (2, 3), "\\Z allows a final newline");
        // `notbol`/`noteol` do not affect `\A`/`\Z` (oracle-verified).
        let strict = Parsed { notbol: true, noteol: true, ..Parsed::default() };
        assert_eq!(span("\\Aa", "abc", &strict), (0, 1));
        assert_eq!(span("c\\Z", "abc\n", &strict), (2, 3));
        assert_eq!(mismatch("^a", "abc", &strict), None);
        // Word boundaries.
        assert_eq!(span("\\babc\\b", "abc", &one), (0, 3));
        assert_eq!(mismatch("\\Babc", "abc", &one), None);
        assert_eq!(span("\\Bb", "abc", &one), (1, 2));
        assert_eq!(span("\\bab", "abc", &one), (0, 2));
        assert_eq!(mismatch("\\Bab", "abc", &one), None);
        assert_eq!(mismatch("b\\b", "abc", &one), None, "c is a word character");
    }

    #[test]
    fn character_classes_and_the_class_escapes() {
        let one = parsed();
        assert_eq!(span("[a-c]+", "abc1", &one), (0, 3));
        assert_eq!(span("\\d+", "abc1", &one), (3, 4));
        assert_eq!(span("\\D+", "abc1", &one), (0, 3));
        assert_eq!(span("[^a-z]+", "abc1", &one), (3, 4));
        // `\w` is the ASCII set the word-boundary rule uses.
        assert_eq!(span("\\w+", "a_1 b", &one), (0, 3));
    }

    #[test]
    fn the_dot_honours_dotall_and_the_newline_convention() {
        let one = parsed();
        let dotall = Parsed {
            compile: CompileFlags { dotall: true, ..CompileFlags::default() },
            ..Parsed::default()
        };
        assert_eq!(mismatch("a.b", "a\nb", &one), None, "`.` refuses a newline");
        assert_eq!(span("a.c", "a\nc", &dotall), (0, 3));
        assert_eq!(span("(?s)a.c", "a\nc", &one), (0, 3), "an inline `(?s)`");
        assert_eq!(span(".", "a\nb", &one), (0, 1));
        assert_eq!(span(".", "a\nb", &dotall), (0, 1), "the first character anyway");
    }

    #[test]
    fn the_global_loop_reproduces_the_oracle_lists() {
        assert_eq!(globe("a", "abab"), vec![(0, 1), (2, 3)]);
        assert_eq!(globe("a*", "aaa"), vec![(0, 3), (3, 3)]);
        assert_eq!(globe("", "abc"), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
        assert_eq!(globe("a*", "baa"), vec![(0, 0), (1, 3), (3, 3)]);
        assert_eq!(globe("a*?", "ab"), vec![(0, 0), (0, 1), (1, 1), (2, 2)]);
        assert_eq!(globe("a*?", "abc"), vec![(0, 0), (0, 1), (1, 1), (2, 2), (3, 3)]);
        assert_eq!(globe("a{0}", "a"), vec![(0, 0), (1, 1)]);
        // The `forward(A, 0)` branch plus the duplicate drop `loopexec` does:
        // without it `{1,0}` would be reported twice.
        assert_eq!(globe("\\b", " ab"), vec![(1, 1), (3, 3)]);
        assert_eq!(globe("\\b", "ab"), vec![(0, 0), (2, 2)]);
        assert_eq!(globe("\\G.", "abcabc"), vec![(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6)]);
        // A subject with nothing to find is the bare `nomatch`, not an empty list.
        let program = compile_flags("z", &CompileFlags::default());
        let options = Parsed { global: true, ..Parsed::default() };
        assert_eq!(run(&program, &subject("abc"), &options, &[0]), Outcome::NoMatch);
    }

    #[test]
    fn the_global_loop_skips_a_crlf_pair_only_under_a_crlf_like_convention() {
        let crlf = Parsed {
            global: true,
            newline_opt: Some(Newline::Crlf),
            ..Parsed::default()
        };
        assert_eq!(spans("", "a\r\nb", &crlf), vec![(0, 0), (1, 1), (3, 3), (4, 4)]);
        let one = Parsed { global: true, ..Parsed::default() };
        assert_eq!(
            spans("", "a\r\nb", &one),
            vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)],
            "the default LF convention has no CRLF pair to skip"
        );
    }

    #[test]
    fn the_global_loop_starts_at_the_run_offset() {
        let options = Parsed { global: true, start_offset: 2, ..Parsed::default() };
        assert_eq!(spans("b", "abcabc", &options), vec![(4, 5)]);
        // `\G` compares against each call's own offset: the assertion holds at
        // every position the loop reaches.
        let options = Parsed { global: true, start_offset: 2, ..Parsed::default() };
        assert_eq!(spans("\\G.", "abcabc", &options), vec![(2, 3), (3, 4), (4, 5), (5, 6)]);
    }

    #[test]
    fn the_empty_match_rules_and_the_empty_loop_guard() {
        let one = parsed();
        assert_eq!(span("", "abc", &one), (0, 0), "the empty pattern matches at 0");
        let atstart = Parsed { notempty_atstart: true, ..Parsed::default() };
        assert_eq!(span("", "abc", &atstart), (1, 1), "only the attempt at 0 is refused");
        let notempty = Parsed { notempty: true, ..Parsed::default() };
        assert_eq!(mismatch("", "abc", &notempty), None);
        assert_eq!(span("a?", "abc", &notempty), (0, 1), "a non-empty match is fine");
        assert_eq!(mismatch("a?", "b", &notempty), None);
        // A loop whose body matches the empty string must terminate (without the
        // guard these would spin).
        assert_eq!(span("(a?)*", "aa", &one), (0, 2));
        assert_eq!(span("(a*)*b", "aab", &one), (0, 3));
        assert_eq!(span("(?:)*", "abc", &one), (0, 0));
        assert_eq!(span("(?:a|b)*", "abab", &one), (0, 4));
    }

    #[test]
    fn the_search_honours_anchored_firstline_and_the_offset() {
        let anchored = Parsed { compile: CompileFlags { anchored: true, ..CompileFlags::default() }, ..Parsed::default() };
        assert_eq!(span("a", "abc", &anchored), (0, 1));
        assert_eq!(mismatch("a", "bac", &anchored), None);
        let firstline =
            Parsed { compile: CompileFlags { firstline: true, ..CompileFlags::default() }, ..Parsed::default() };
        assert_eq!(mismatch("b", "xyz\nb", &firstline), None);
        assert_eq!(span("b", "xyz\nb", &parsed()), (4, 5));
        let offset = Parsed { start_offset: 2, ..Parsed::default() };
        assert_eq!(span("b", "abcabc", &offset), (4, 5));
        assert_eq!(mismatch("a", "abc", &offset), None, "nothing after the offset");
        // An offset past the end is simply no match.
        let past = Parsed { start_offset: 99, ..Parsed::default() };
        assert_eq!(mismatch("a", "abc", &past), None);
    }

    #[test]
    fn a_match_limit_of_zero_exhausts_immediately() {
        let options = Parsed { match_limit: Some(0), ..Parsed::default() };
        let program = compile_flags("a", &CompileFlags::default());
        assert_eq!(run(&program, &subject("abc"), &options, &[0]), Outcome::MatchLimit);
        // It is a limit, not "no match": a pattern that cannot match at all
        // still reports the exhaustion.
        let program = compile_flags("z", &CompileFlags::default());
        assert_eq!(run(&program, &subject("abc"), &options, &[0]), Outcome::MatchLimit);
        // A limit that is never reached does not interfere.
        let generous = Parsed { match_limit: Some(1_000), ..Parsed::default() };
        assert_eq!(span("a", "abc", &generous), (0, 1));
        // A budget of one is also oracle-exact for a literal pattern:
        // `re:run("abc","a",[{match_limit,1},report_errors])` = `{error,match_limit}`.
        let one = Parsed { match_limit: Some(1), ..Parsed::default() };
        let program = compile_flags("a", &CompileFlags::default());
        assert_eq!(run(&program, &subject("abc"), &one, &[0]), Outcome::MatchLimit);
    }

    #[test]
    fn a_utf8_subject_is_validated_and_keeps_codepoint_offsets() {
        assert!(matches!(Subject::utf8(vec![0x80]), Err(beam_common::BeamError::BadArg)));
        assert!(
            matches!(Subject::utf8(vec![0xc3]), Err(beam_common::BeamError::BadArg)),
            "a truncated sequence"
        );
        assert!(
            matches!(Subject::utf8(vec![0xed, 0xa0, 0x80]), Err(beam_common::BeamError::BadArg)),
            "a surrogate is not valid UTF-8"
        );
        let subject = Subject::utf8("é".as_bytes().to_vec()).expect("valid UTF-8");
        assert_eq!(subject.codes, vec![0xE9]);
        assert_eq!(subject.offs, vec![0, 2]);
        assert_eq!(subject.byte_span((0, 1)), (0, 2), "the byte span of one codepoint");
        assert_eq!(subject.codepoints((0, 1)), &[0xE9]);
        // A latin-1 subject reads the same bytes as two codepoints.
        let latin1 = Subject::latin1("é".as_bytes().to_vec());
        assert_eq!(latin1.codes, vec![0xC3, 0xA9]);
        assert_eq!(latin1.byte_span((0, 1)), (0, 1));
        assert_eq!(latin1.codepoints((1, 2)), &[0xA9]);
        // Out-of-range spans clamp instead of panicking.
        assert_eq!(latin1.byte_span((0, 99)), (0, 2));
        assert!(latin1.codepoints((5, 99)).is_empty());
    }

    #[test]
    fn a_long_subject_does_not_overflow_the_stack() {
        // 20 000 codepoints through a quantifier, a class and a backreference —
        // every VM loop is iterative, so this must not recurse per element.
        let text = "a".repeat(20_000);
        assert_eq!(span("a+", &text, &parsed()), (0, 20_000));
        assert_eq!(span("a*", &text, &parsed()), (0, 20_000));
        assert_eq!(span("[a]+", &text, &parsed()), (0, 20_000));
        assert_eq!(span("(a)+", &text, &parsed()), (0, 20_000));
        assert_eq!(span("a+a+", &text, &parsed()), (0, 20_000));
        let global = Parsed { global: true, ..Parsed::default() };
        assert_eq!(spans("a", &text, &global).len(), 20_000);
        let long = format!("{}b", "a".repeat(20_000));
        assert_eq!(span("a*b", &long, &parsed()), (0, 20_001));
    }
}
