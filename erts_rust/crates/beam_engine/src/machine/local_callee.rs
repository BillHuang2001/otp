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

//! The **local-call verdict table**: the host's answer to
//! [`ExecutionContext::resolve_local_call`](crate::ExecutionContext::resolve_local_call)
//! for one same-module callee, addressed **directly by the callee's function index**.
//!
//! [`Machine::local_call_seam`](crate::Machine) runs for every local
//! `call`/`call_only`/`call_last` and has to answer one question: **does the host serve
//! this same-module callee as a BIF** ([`Machine::local_call_seam`](crate::Machine))?
//! The answer cannot change on its own: the host's verdict for a fixed
//! `(module, function, arity)` triple is a pure function of that triple — `beam_rt`'s
//! implementation is `prim_tty::serves_local_call` then `beam_bifs::serves_local_call`
//! (the `zlib` stub table), neither of which reads run state — and the function table and
//! atom table it is derived from are built once at load time. So a repeat local call is
//! **one bounds-checked slot load**: no loop, no key comparison, no virtual call, no
//! string comparison.
//!
//! # Why caching the verdict is the faithful choice, not a shortcut
//!
//! ERTS charges **no reduction at all** for an intra-module call: `i_call` stores the
//! continuation and jumps (`../../erts/emulator/beam/emu/instrs.tab`), and reductions are
//! counted per function head/`receive`, not per call. This seam is an engine artefact —
//! the host, not the VM, is on the other end — so the earlier policy of paying one host
//! consultation (a reduction unit in `beam_rt`) on **every** local call was strictly
//! *less* faithful than memoizing: it priced an implementation detail into the workload.
//! `list_walk`'s inner loop makes two local calls per walked element, which is why the
//! consultation showed up as ~11 % of that run's self time.
//!
//! # Why an index-addressed table, and not a key scan
//!
//! The first two versions of this cache were **searches**: a linear scan of up to eight
//! `(module address, function index)` slots, first with a move-to-front policy and then
//! with "insert-at-front on a miss, a hit never reorders". Both were measured, and both
//! are small object lessons in what a hot local-call path may not contain:
//!
//! * the **MRU version was actively wrong** for this workload: `list_walk` alternates two
//!   callees (`sum/1` and `walk/3`), so a move-to-front fires on *every* call — a
//!   `slice::rotate_right(1)` memmove of up to eight owned entries in the innermost loop;
//! * the **fixed-scan version** removed the memmove but kept a **data-dependent loop**:
//!   the same alternation exits the scan after one slot on every other call and after two
//!   on the rest, i.e. one **unpredictable branch per local call**. That is exactly the
//!   signature the interpreter tier showed — instructions/element flat (+0.37 %) while
//!   cycles/element rose (+7.7 %) — a mispredict/stall cost, not extra work.
//!
//! Indexing by the callee's own function index removes the decision entirely: the slot a
//! callee uses is a property of the *callee*, not of the call history, so the load is the
//! same instruction with the same latency on every call and the branch predictor has
//! nothing to guess.
//!
//! # Where a table lives, and why its identity is free
//!
//! One table per **module context** ([`ModuleContext`](crate::machine::ModuleContext)):
//! a registered context owns the table of the `Arc<Module>` it was registered with, and
//! the run's entry module (context `0`, which the machine never owns — the drive loop
//! borrows it) has its table in [`Machine::entry_verdicts`](crate::Machine). So the
//! callee's "which module is this function in?" half of the old key is the context
//! itself, and it cannot be got wrong: a `Module::clone` or a **separate load of the same
//! `.beam`** is a different image, therefore a different context ([`Contexts`] dedups by
//! `Arc::ptr_eq`) and therefore a **different table** — one image's `Served` can never be
//! read as another's. [`LocalVerdicts::describes`] additionally records the image address
//! the table was sized for, which the seam asserts in debug builds and which is what
//! re-validates the entry table on every run entry.
//!
//! The slots are [`Cell`]s because the seam reads the table while the machine also holds
//! `&Module` borrows of the context's module (and, for a registered context, through a
//! shared `Arc`); the seam itself is `&mut self`.
//!
//! # Invalidation
//!
//! The one documented way a host's answer set changes **mid-run** is
//! `erlang:load_nif/2` installing a NIF for a stub a later local call must reach. The
//! BIF-dispatch funnel therefore **arms** an invalidation when it dispatches
//! `("erlang", "load_nif", 2)` — a flag write through `&self`
//! ([`Machine::local_call_invalidation_pending`]), because the dispatch door is `&self` —
//! and the seam consumes it at its **next** local call, where it is `&mut self` and calls
//! [`Machine::invalidate_local_call_verdicts`]. That door clears **every** context's
//! table (the entry module's and each registered one) back to [`Verdict::Unknown`], which
//! is the same granularity the key scan had: a verdict, not an identity. An un-nameable
//! callee stays [`Verdict::Unnameable`] — there is no name to ask about.
//! [`Machine::reset`](crate::Machine) drops the whole set at the start of every run. A
//! host whose answer set changes by any **other** means (its own plugins, a reload, a
//! configuration toggle) calls [`Machine::invalidate_local_call_verdicts`] itself — that
//! is the door's purpose, and it is public for exactly that reason.

use std::cell::Cell;

use beam_loader::Module;

/// What the table knows about one callee's host verdict — the byte the local-call seam's
/// hot path loads.
///
/// It is `Copy` and one byte, so a slot hands it back by value with no borrow of the
/// table (which is what lets the same method consult the host, a separate
/// `&mut dyn ExecutionContext`, without fighting the borrow checker).
///
/// The discriminants are **explicit and ordered** for one reason: the two verdicts that
/// leave the caller's in-machine jump are the two lowest, so
/// [`Verdict::enters_in_machine`] — the seam's hot test, run on every local call — is a
/// single range compare. Written as a `match`, the four-way test compiled to a
/// **jump table** (a table load plus an indirect jump per local call, measured at ~64 % of
/// the seam's own cycles on `gc_bound/churn`); the range test cannot be lowered that way,
/// and [`Verdict::enters_in_machine_is_the_low_discriminant_range`] pins the ordering it
/// depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Verdict {
    /// The host answered `false`: enter the callee in this machine.
    Declined = 0,
    /// The callee cannot be named (no function entry, or a name atom the module's atom
    /// table does not resolve), so the host is **never** asked about it.
    Unnameable = 1,
    /// The host answered `true`: serve this callee as a BIF.
    Served = 2,
    /// Nothing is known about this callee yet (its slot's value at creation, and what an
    /// **invalidated** verdict falls back to): the seam derives the callee's identity and
    /// consults the host once, then records the answer.
    ///
    /// It is also the answer for a slot the table does not have — an unbuilt table (a
    /// machine stepped without a run entry) or a function index the module does not own.
    /// Neither can happen for a well-formed local call, and `Unknown` is the one answer
    /// that cannot fabricate a verdict: it can only re-ask.
    Unknown = 3,
}
impl Verdict {
    /// The first discriminant that does **not** enter the callee in this machine — the
    /// upper bound of the range [`Verdict::enters_in_machine`] tests.
    const FIRST_NOT_IN_MACHINE: u8 = Verdict::Served as u8;
    /// Does this verdict keep the caller's unchanged **in-machine jump** (so the host
    /// needs no dispatch and the seam answers `Ok(None)`)?
    ///
    /// `true` for the two verdicts that mean "run the callee's own code": a memoized
    /// decline (the host's own earlier answer) and a memoized un-nameable callee (a callee
    /// the host is never asked about). One range compare, no jump table.
    #[inline(always)]
    pub(crate) fn enters_in_machine(self) -> bool {
        (self as u8) < Self::FIRST_NOT_IN_MACHINE
    }
}

/// One module context's **index-addressed** local-call verdict table: one slot per entry
/// of [`Module::functions`], read by the callee's function index.
///
/// A table covers **one** module image ([`LocalVerdicts::owner`]) and is created with
/// that image's function count, so a slot index is a function index in *that* module and
/// nothing else. It is filled with [`Verdict::Unknown`] at creation and never resized,
/// which is what makes the seam's read a single bounds-checked load.
pub(crate) struct LocalVerdicts {
    /// The address of the module image the table was sized for — the **identity
    /// witness** [`LocalVerdicts::describes`] checks.
    owner: usize,
    /// One slot per function, `Verdict::Unknown` at creation.
    table: Box<[Cell<Verdict>]>,
    /// Test-only: how many times the seam **derived** a callee identity for this table
    /// (the once-per-callee path), so a test can prove the derivation count
    /// deterministically. Absent from non-test builds.
    #[cfg(test)]
    derivations: Cell<u64>,
}

impl LocalVerdicts {
    /// A table with **no** slots and no owner — the state of the entry table before a run
    /// binds its module, and the state [`Machine::reset`](crate::Machine) leaves it in.
    ///
    /// Every read of such a table answers [`Verdict::Unknown`] (so a machine stepped
    /// outside a run consults the host instead of inventing a verdict) and every write is
    /// a no-op, exactly like a cache that is not collecting.
    pub(crate) fn empty() -> LocalVerdicts {
        LocalVerdicts {
            owner: 0,
            table: Vec::new().into_boxed_slice(),
            #[cfg(test)]
            derivations: Cell::new(0),
        }
    }

    /// A fresh all-[`Verdict::Unknown`] table sized for `module`'s function table — the
    /// table of the module context `module` names.
    pub(crate) fn for_module(module: &Module) -> LocalVerdicts {
        LocalVerdicts {
            owner: module as *const Module as usize,
            table: vec![Cell::new(Verdict::Unknown); module.functions.len()].into_boxed_slice(),
            #[cfg(test)]
            derivations: Cell::new(0),
        }
    }

    /// Does this table belong to `module` — the same image, with the same function count?
    ///
    /// The identity witness: a table is only ever valid for the module image it was sized
    /// for, so the entry table (the one not owned by a `ModuleContext`) is re-validated
    /// against the run's entry module at every run entry, and the seam asserts this in
    /// debug builds. The count is compared as well so that an address reused by a
    /// differently shaped image can never pass (address reuse alone is already impossible
    /// while the run holds the module, but the check must not depend on that).
    #[inline(always)]
    pub(crate) fn describes(&self, module: &Module) -> bool {
        self.owner == module as *const Module as usize && self.table.len() == module.functions.len()
    }

    /// The memo's answer for `callee` — the seam's **hot path**, and the whole reason this
    /// type is index-addressed.
    ///
    /// One bounds-checked load: `callee` is the callee's [`Module::functions`] index, so
    /// the slot is a property of the callee and the read has the same latency and takes the
    /// same branch on every call — an alternating two-callee loop (what `list_walk`'s inner
    /// loop is) costs exactly what a single-callee loop costs. A missing slot answers
    /// [`Verdict::Unknown`], never a fabricated verdict.
    #[inline(always)]
    pub(crate) fn get(&self, callee: u32) -> Verdict {
        match self.table.get(callee as usize) {
            Some(slot) => slot.get(),
            None => Verdict::Unknown,
        }
    }
    /// Does the table's answer for `callee` keep the caller's **in-machine jump** —
    /// [`Verdict::enters_in_machine`] folded into the slot read?
    ///
    /// This is the whole of the seam's hot path: one bounds-checked `Cell` load and one
    /// range compare (a missing slot answers `false`, i.e. "ask the host", the same
    /// [`Verdict::Unknown`] the full read would have answered).
    #[inline(always)]
    pub(crate) fn enters_in_machine(&self, callee: u32) -> bool {
        match self.table.get(callee as usize) {
            Some(slot) => slot.get().enters_in_machine(),
            None => false,
        }
    }

    /// Record `verdict` for `callee`.
    ///
    /// A missing slot is left alone: the seam only ever records for an index it just read
    /// (which the identity derivation proved is a real function entry), so refusing here
    /// keeps a fabricated entry out of the table.
    #[inline]
    pub(crate) fn set(&self, callee: u32, verdict: Verdict) {
        if let Some(slot) = self.table.get(callee as usize) {
            slot.set(verdict);
        }
    }

    /// Forget every host verdict (**keeping** nothing else — the identity is never stored,
    /// it is re-derived on demand), so the next local call to every callee consults the
    /// host again.
    ///
    /// This is what `erlang:load_nif/2` needs (a stub a later local call must reach) and
    /// what a host with a stateful answer set calls through
    /// [`Machine::invalidate_local_call_verdicts`](crate::Machine). One sequential walk of
    /// one byte per function, reached from a BIF dispatch or from the seam's next local
    /// call, so it is never on a hot path; an un-nameable callee keeps its verdict (there
    /// is no name to ask about).
    pub(crate) fn invalidate(&self) {
        for slot in self.table.iter() {
            if slot.get() != Verdict::Unnameable {
                slot.set(Verdict::Unknown);
            }
        }
    }

    /// Drop every slot and the owner (`Machine::reset` — the tables belong to the run that
    /// filed them).
    ///
    /// Once per run, so a plain `mem::replace` of the whole value is the whole cost.
    pub(crate) fn clear(&mut self) {
        *self = LocalVerdicts::empty();
    }

    /// The number of slots (one per function of the module image the table describes).
    #[cfg(test)]
    pub(crate) fn slots(&self) -> usize {
        self.table.len()
    }

    /// Test-only: one identity derivation was spent on this table.
    #[cfg(test)]
    pub(crate) fn note_derivation(&self) {
        self.derivations.set(self.derivations.get() + 1);
    }

    /// The number of identity derivations this table has paid (test-only).
    #[cfg(test)]
    pub(crate) fn derivation_count(&self) -> u64 {
        self.derivations.get()
    }
}

impl Default for LocalVerdicts {
    fn default() -> LocalVerdicts {
        LocalVerdicts::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::{LocalVerdicts, Verdict};
    use beam_loader::Module;

    /// A module with `names.len()` one-arity functions, built through the shared
    /// synthetic-module fixture.
    fn fixture(names: &[&str]) -> Module {
        let mut fixture = Fixture::new("m");
        for name in names {
            fixture.add_function(name, 1, Vec::new());
        }
        fixture.finish()
    }

    /// A table is sized from the module's function table and starts **unknown**: a fresh
    /// table holds no opinion about any callee, so the first call derives and asks.
    #[test]
    fn a_new_table_has_one_unknown_slot_per_function() {
        let module = fixture(&["a", "b", "c"]);
        let table = LocalVerdicts::for_module(&module);

        assert_eq!(table.slots(), module.functions.len());
        assert!((0..3).all(|func| table.get(func) == Verdict::Unknown));
        assert!(table.describes(&module));
    }

    /// The hot test is a **range** compare over the discriminants, so the two
    /// enter-in-machine verdicts must stay the two lowest and every other verdict must stay
    /// above them. This pins the ordering the lowering depends on: reorder or insert a
    /// variant and this fails rather than silently turning a decline into a host call (or,
    /// worse, a served callee into an in-machine jump).
    #[test]
    fn enters_in_machine_is_the_low_discriminant_range() {
        assert!(Verdict::Declined.enters_in_machine(), "a decline runs the callee");
        assert!(
            Verdict::Unnameable.enters_in_machine(),
            "an un-nameable callee runs the callee"
        );
        assert!(
            !Verdict::Served.enters_in_machine(),
            "a served callee is a BIF"
        );
        assert!(
            !Verdict::Unknown.enters_in_machine(),
            "an unknown callee must ask the host"
        );
        // The range bound is exactly the first verdict that does not enter in machine.
        assert_eq!(
            Verdict::FIRST_NOT_IN_MACHINE,
            Verdict::Served as u8,
            "the range test's bound is `Served`"
        );
        assert!(
            (Verdict::Declined as u8) < (Verdict::Unnameable as u8)
                && (Verdict::Unnameable as u8) < Verdict::FIRST_NOT_IN_MACHINE,
            "the two enter-in-machine verdicts are the two lowest discriminants, in order"
        );
    }

    /// The slot-read fold ([`LocalVerdicts::enters_in_machine`]) answers exactly what the
    /// full read followed by [`Verdict::enters_in_machine`] answers — for every verdict,
    /// and for a callee with no slot at all.
    #[test]
    fn the_folded_slot_read_agrees_with_the_full_read() {
        let module = fixture(&["a", "b"]);
        let table = LocalVerdicts::for_module(&module);

        for verdict in [
            Verdict::Declined,
            Verdict::Unnameable,
            Verdict::Served,
            Verdict::Unknown,
        ] {
            table.set(1, verdict);
            assert_eq!(
                table.enters_in_machine(1),
                table.get(1).enters_in_machine(),
                "the folded read for a memoized {verdict:?} verdict"
            );
            assert_eq!(table.enters_in_machine(1), verdict.enters_in_machine());
        }
        assert_eq!(
            table.enters_in_machine(1),
            table.get(1).enters_in_machine(),
            "… and the same agreement for the last value written"
        );
        // No slot: the fold answers `false` (ask the host), like the full read's `Unknown`.
        assert!(!table.enters_in_machine(7));
        assert_eq!(table.get(7), Verdict::Unknown);
        // An unbuilt table never claims a callee enters in machine.
        assert!(!LocalVerdicts::empty().enters_in_machine(0));
        assert_eq!(table.derivation_count(), 0, "neither read asks anything");
    }

    /// The table is **index-addressed**: a callee's verdict is read back from the slot its
    /// own function index names, and recording one callee never disturbs another.
    #[test]
    fn a_verdict_is_read_back_from_the_callees_own_index() {
        let module = fixture(&["a", "b"]);
        let table = LocalVerdicts::for_module(&module);

        table.set(1, Verdict::Served);
        table.set(0, Verdict::Declined);
        assert_eq!(table.get(1), Verdict::Served);
        assert_eq!(table.get(0), Verdict::Declined);
        assert_eq!(table.derivation_count(), 0, "the table asks nothing itself");
    }

    /// A slot the table does not have answers [`Verdict::Unknown`] (so the seam re-derives
    /// and re-asks) and refuses the write: a fabricated index can never leave a verdict
    /// behind.
    #[test]
    fn an_out_of_range_callee_is_unknown_and_unwritable() {
        let module = fixture(&["a"]);
        let table = LocalVerdicts::for_module(&module);

        assert_eq!(table.get(7), Verdict::Unknown);
        table.set(7, Verdict::Served);
        assert_eq!(table.get(7), Verdict::Unknown, "nothing was recorded");
    }

    /// An **unbuilt** table behaves like an empty cache, never like a full one: every read
    /// is unknown (consult the host) and every write is dropped.
    #[test]
    fn an_empty_table_answers_unknown_and_drops_writes() {
        let mut table = LocalVerdicts::empty();
        let module = fixture(&["a"]);

        assert_eq!(table.slots(), 0);
        assert!(!table.describes(&module), "an empty table owns no module");
        assert_eq!(table.get(0), Verdict::Unknown);
        table.set(0, Verdict::Served);
        assert_eq!(table.get(0), Verdict::Unknown);
        table.clear();
        assert_eq!(table.get(0), Verdict::Unknown);
    }

    /// Invalidation clears **verdicts**: a served or declined callee is re-asked, an
    /// un-nameable one keeps its verdict (there is no name to ask about), and a callee that
    /// was never asked stays unknown.
    #[test]
    fn invalidation_resets_every_verdict_but_the_unnameable_one() {
        let module = fixture(&["a", "b", "c", "d"]);
        let table = LocalVerdicts::for_module(&module);

        table.set(0, Verdict::Served);
        table.set(1, Verdict::Declined);
        table.set(2, Verdict::Unnameable);
        // 3 stays `Unknown`.
        table.invalidate();
        assert_eq!(table.get(0), Verdict::Unknown);
        assert_eq!(table.get(1), Verdict::Unknown);
        assert_eq!(
            table.get(2),
            Verdict::Unnameable,
            "an un-nameable callee stays unnameable"
        );
        assert_eq!(table.get(3), Verdict::Unknown);
    }

    /// The identity witness compares **both** halves: a different image of the same shape
    /// and the same image with a different shape are both rejected, so a table can never be
    /// read for a module it was not built for.
    #[test]
    fn describes_demands_the_same_image_and_the_same_function_count() {
        let mut module = fixture(&["a", "b"]);
        let twin = fixture(&["a", "b"]);
        let table = LocalVerdicts::for_module(&module);

        assert!(table.describes(&module));
        assert!(
            !table.describes(&twin),
            "a separately loaded image is a different table"
        );
        let extra = module.functions[0].clone();
        module.functions.push(extra);
        assert!(
            !table.describes(&module),
            "the same address with a different function count is rejected too"
        );
    }
}
