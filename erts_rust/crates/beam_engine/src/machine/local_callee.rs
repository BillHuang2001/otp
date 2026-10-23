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

//! The **local-call memo** behind [`Machine::local_callees`]: the callee identity the
//! local-call seam needs, plus the host's verdict for it.
//!
//! [`Machine::local_call_seam`](crate::Machine) has to answer two questions for every
//! local `call`/`call_only`/`call_last`:
//!
//! 1. **who** is the same-module callee — its `(module name, function name, arity)` —
//!    which it derives from the callee's **own** function entry
//!    ([`Module::functions`]) and two of the running module's atom names, and
//! 2. **does the host serve it** as a BIF
//!    ([`ExecutionContext::resolve_local_call`](crate::ExecutionContext::resolve_local_call)).
//!
//! Both answers are memoized here, both keyed on `(module image address, function
//! index)`. Neither answer can go stale on its own: the function table and the atom
//! table are built once at load time and never mutated afterwards, and the host's
//! verdict for a fixed `(module, function, arity)` triple is a pure function of that
//! triple — `beam_rt`'s implementation is `prim_tty::serves_local_call` then
//! `beam_bifs::serves_local_call` (the `zlib` stub table), neither of which reads run
//! state. A repeat local call is therefore **one fixed key scan plus one byte load**,
//! instead of a virtual call and a string comparison per call.
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
//! consultation showed up as ~11 % of that run's self time (`resolve_local_call` in both
//! of `beam_rt`'s contexts plus this memo's own scan).
//!
//! # Invalidation
//!
//! The one documented way a host's answer set changes **mid-run** is
//! `erlang:load_nif/2` installing a NIF for a stub a later local call must reach. The
//! BIF-dispatch funnel therefore clears every verdict when it dispatches
//! `("erlang", "load_nif", 2)` ([`Machine::invalidate_local_call_verdicts`], reached
//! from `Machine::dispatch_bif`), and [`Machine::reset`](crate::Machine) clears the whole
//! memo at the start of every run. A host whose answer set changes by any **other**
//! means (its own plugins, a reload, a configuration toggle) calls
//! [`Machine::invalidate_local_call_verdicts`] itself — that is the door's purpose, and
//! it is public for exactly that reason.
//!
//! An **un-nameable** callee is memoized as [`Verdict::Unnameable`] and keeps costing the
//! host **zero** consultations (the seam never asks about a fabricated name).
//!
//! # The replacement policy: insert-at-front on a miss, a hit never reorders
//!
//! The first version of this memo moved a hit to the front (an MRU cache). That is
//! exactly the wrong policy here, and measurably so: the workload this seam is priced on
//! (`list_walk`, `src/bench/list_walk.rs`) makes **two** local calls per walked element
//! (`sum/1` and `walk/3`), so it **alternates** two callees and a move-to-front fires on
//! *every* call — a `slice::rotate_right(1)` memmove of up to eight owned entries in the
//! innermost loop. The hot path is now what it was meant to be:
//!
//! * **a hit reads and reorders nothing** — the entry stays where it is, so an
//!   alternating (or round-robin) call pattern is all hits after its first cycle;
//! * **only a miss inserts**, at the front, shifting the occupied prefix one slot back
//!   so the **oldest insertion** falls off the end. Misses happen once per
//!   `(module, function index)` per run, so their cost is irrelevant — a `rotate_right`
//!   there is fine.
//!
//! # Why the scan is cheap
//!
//! The storage is split into three parallel arrays — a compact `keys: [CalleeKey; CAP]`
//! (16 bytes per entry, so the whole key array is two cache lines), a `verdicts:
//! [Verdict; CAP]` byte array (one cache line, and the **only** thing the hot path reads
//! besides the keys) and the `targets: [Option<CalleeTarget>; CAP]` payloads, which own
//! their name text and are touched only on the cold paths (a fill, or a call the host
//! actually serves). The scan covers only the **occupied prefix** `keys[..len]`, so the
//! steady state costs one comparison per distinct callee the module actually calls —
//! `list_walk`'s two-callee inner loop scans two slots in one cache line. (The vacant
//! slots are keyed on [`CalleeKey::VACANT`], which [`CalleeKey::of`] can never produce,
//! so the bound is an economy, not a validity argument.)
//!
//! **Why the key is the module's address.** The entry belongs to a `Module` image the
//! machine is executing, so the cheapest identity for it is its address. Address reuse
//! (a freed image whose address a new one inherits) cannot produce a stale hit: the
//! memo is cleared by [`Machine::reset`](crate::Machine) — the start of every run — and
//! every module a run entered is kept alive for the whole run by the module-context
//! registry ([`Contexts`](crate::machine::Contexts)) or by the driver's own borrow of
//! the entry module, so no address in the memo can be recycled while the memo holds
//! it. A resume deliberately does **not** reset, and a suspended machine's modules are
//! still alive for the same reason.
//!
//! The **name text** is owned ([`Box<str>`]), so nothing here borrows a module; and the
//! atom *table* deliberately does not participate in the key or the validity argument:
//! an [`AtomTable`](beam_term::AtomTable) is append-only, so an atom index resolved
//! once can never be re-pointed at a different name.

use beam_loader::Module;

/// The number of memoized callees one machine keeps.
///
/// A machine executes one module at a time and a module's local calls reach a handful
/// of distinct callees in any one workload, so a fixed, allocation-free array is
/// enough; the eviction policy (the **oldest insertion** falls off the end) makes a
/// cold callee merely cost one re-derivation and one re-consultation, never a wrong
/// answer.
const CAP: usize = 8;

/// What the memo knows about one callee's host verdict — the byte the local-call seam's
/// hot path reads.
///
/// It is `Copy` and one byte, so the seam's hot path hands it back by value with no
/// borrow of the memo (which is what lets the same method consult the host, a separate
/// `&mut dyn ExecutionContext`, without fighting the borrow checker).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The callee cannot be named (no function entry, or a name atom the module's atom
    /// table does not resolve), so the host is **never** asked about it.
    ///
    /// This doubles as the value in a vacant slot, which the scan can never match.
    Unnameable,
    /// The host has not been asked yet (or its earlier answer was invalidated).
    Ask,
    /// The host answered `true`: serve this callee as a BIF.
    Served,
    /// The host answered `false`: enter the callee in this machine.
    Declined,
}

/// One memoized callee identity: exactly what
/// [`ExecutionContext::resolve_local_call`](crate::ExecutionContext::resolve_local_call)
/// is asked about, with the name text **owned** so the memo outlives every borrow of
/// the module it came from.
///
/// It mirrors the cold-path derivation
/// ([`local_callee_identity`](crate::interpreter)) value for value: the callee's
/// **own** function entry names it, never the call site's operands and never the
/// module's own name atom, and `arity` is the callee's **declared** arity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CalleeTarget {
    /// The module the callee's function entry belongs to.
    pub module: Box<str>,
    /// The callee's function name.
    pub function: Box<str>,
    /// The callee's declared arity.
    pub arity: u32,
}

/// The identity of one memo slot: the **address** of the module image the entry
/// belongs to plus the function index within it.
///
/// Both halves are immutable for a loaded module (see the module docs for why the
/// address is a sound key), and both are trivially copied, which is what makes the
/// scan array a flat 16-byte record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CalleeKey {
    /// The address of the `Module` the function lives in.
    pub module: usize,
    /// The function index ([`Module::functions`] index).
    pub func: u32,
}

impl CalleeKey {
    /// The key of `func` in `module`.
    #[inline(always)]
    fn of(module: &Module, func: u32) -> CalleeKey {
        CalleeKey {
            module: module as *const Module as usize,
            func,
        }
    }

    /// The vacant slot's key: a real module reference is never null, so no key
    /// [`CalleeKey::of`] builds can ever equal this one.
    const VACANT: CalleeKey = CalleeKey { module: 0, func: 0 };
}

/// The machine's **local-call memo**: a fixed, allocation-free cache of the callee
/// identity and the host's verdict, both keyed on `(module address, function index)`.
///
/// `keys[..len]`, `verdicts[..len]` and `targets[..len]` are the occupied slots, front
/// (most recent insertion) first; a slot past `len` is vacant (`keys` =
/// [`CalleeKey::VACANT`], `verdicts` = [`Verdict::Unnameable`], `targets` = `None`). The
/// split is what keeps the hot scan on the keys and the verdict bytes: the payloads own
/// their name text and are touched only once a key has matched **and** the slot still
/// has to be filled or served.
pub(crate) struct LocalCallees {
    /// The slot keys — the whole hot scan reads only this array.
    keys: [CalleeKey; CAP],
    /// The host's verdict per slot (and the un-nameable flag) — the byte the hot path
    /// reads after the key matched.
    verdicts: [Verdict; CAP],
    /// The memoized identities, parallel to `keys`.
    targets: [Option<CalleeTarget>; CAP],
    /// The occupied prefix length (`0..=CAP`): the bound of the hot scan, the span the
    /// insert shift rotates, and the extent the test-only accessors read.
    len: usize,
    /// Test-only: how many times an identity was **derived** (a fill), so a test can
    /// prove the memoization count deterministically. Absent from non-test builds.
    #[cfg(test)]
    fills: u64,
    /// Test-only: how many verdicts were **recorded** (i.e. how many times the seam was
    /// allowed to consult the host), the counterpart of [`LocalCallees::fills`].
    #[cfg(test)]
    asks: u64,
}

impl LocalCallees {
    /// An empty cache.
    pub(crate) fn new() -> LocalCallees {
        LocalCallees {
            keys: [CalleeKey::VACANT; CAP],
            verdicts: [Verdict::Unnameable; CAP],
            targets: std::array::from_fn(|_| None),
            len: 0,
            #[cfg(test)]
            fills: 0,
            #[cfg(test)]
            asks: 0,
        }
    }

    /// The memo's answer for `func` in `module`, deriving the identity on a miss.
    ///
    /// This is the seam's **hot path**, so it answers with a [`Verdict`] — a byte —
    /// rather than a borrow of the payload: [`Verdict::Unnameable`] and
    /// [`Verdict::Declined`] both mean "decline without asking the host",
    /// [`Verdict::Served`] means "dispatch it", and [`Verdict::Ask`] is the once-per-callee
    /// case that must consult [`ExecutionContext::resolve_local_call`] and then
    /// [`LocalCallees::record`] the answer.
    ///
    /// **A hit reorders nothing** (the entry keeps its slot, which is what makes an
    /// alternating-callee loop all hits after its first cycle); **a miss** derives once
    /// and inserts at the front, letting the oldest insertion fall off the end when the
    /// cache is full. `#[inline(always)]`: the body is one fixed scan, so a call that
    /// stayed out of line would cost more than the derivation it saves.
    #[inline(always)]
    pub(crate) fn lookup(&mut self, module: &Module, func: u32) -> Verdict {
        let key = CalleeKey::of(module, func);
        // A hit is a read: the slot the key already occupies answers, and nothing moves.
        if let Some(index) = self.position(key) {
            return self.verdicts[index];
        }
        self.insert(key, derive(module, func));
        self.verdicts[0]
    }

    /// The memoized identity of `func` in `module`, if the slot holds one.
    ///
    /// A second key scan, so only the **cold** paths call it: the once-per-callee host
    /// consultation (it needs the names) and the served dispatch. `None` is either an
    /// un-nameable callee or a key the memo does not hold.
    #[inline]
    pub(crate) fn target(&self, module: &Module, func: u32) -> Option<&CalleeTarget> {
        let key = CalleeKey::of(module, func);
        self.position(key)
            .and_then(|index| self.targets[index].as_ref())
    }

    /// Store the host's verdict for `func` in `module` (`true` = served).
    ///
    /// A callee the memo does not hold, or one it holds as un-nameable, is left exactly
    /// as it is: the seam consulted the host about a name it derived from the memo, so
    /// the key is always present on this path; refusing to record elsewhere keeps a
    /// fabricated entry out of the memo.
    #[inline]
    pub(crate) fn record(&mut self, module: &Module, func: u32, served: bool) {
        let key = CalleeKey::of(module, func);
        if let Some(index) = self.position(key) {
            if self.verdicts[index] != Verdict::Unnameable {
                self.verdicts[index] = if served {
                    Verdict::Served
                } else {
                    Verdict::Declined
                };
            }
        }
        #[cfg(test)]
        {
            self.asks += 1;
        }
    }

    /// Forget every host verdict while **keeping** the memoized identities.
    ///
    /// This is what `erlang:load_nif/2` needs (a stub a later local call must reach) and
    /// what a host with a stateful answer set calls through
    /// [`Machine::invalidate_local_call_verdicts`](crate::Machine): the next local call
    /// to every memoized callee consults the host again, but nothing has to be
    /// re-derived (the identities never go stale). A full array walk of eight bytes, so
    /// it is cheap enough to be reached from a BIF dispatch.
    #[inline]
    pub(crate) fn invalidate_verdicts(&mut self) {
        for verdict in self.verdicts.iter_mut() {
            if *verdict != Verdict::Unnameable {
                *verdict = Verdict::Ask;
            }
        }
    }

    /// Drop every memoized entry (`Machine::reset` — the address-keyed entries are only
    /// valid for the run that filled them).
    ///
    /// Once per run, so a plain loop over the (fixed) payload array is the whole cost —
    /// it is deliberately not on any hot path.
    pub(crate) fn clear(&mut self) {
        for slot in self.targets.iter_mut() {
            *slot = None;
        }
        self.keys = [CalleeKey::VACANT; CAP];
        self.verdicts = [Verdict::Unnameable; CAP];
        self.len = 0;
    }

    /// The number of identity derivations this machine has performed (test-only).
    #[cfg(test)]
    pub(crate) fn fill_count(&self) -> u64 {
        self.fills
    }

    /// The number of verdicts this machine has recorded — i.e. how many host
    /// consultations the seam has spent (test-only).
    #[cfg(test)]
    pub(crate) fn ask_count(&self) -> u64 {
        self.asks
    }

    /// The verdict recorded for `key`, without filling anything (test-only).
    #[cfg(test)]
    pub(crate) fn verdict(&self, key: CalleeKey) -> Option<Verdict> {
        self.position(key).map(|index| self.verdicts[index])
    }

    /// The memoized identity under `key`, without filling anything (test-only: it is how
    /// a test asserts *where* the memo put an identity, and that the key is the module
    /// image's address).
    #[cfg(test)]
    pub(crate) fn memoized(&self, key: CalleeKey) -> Option<&CalleeTarget> {
        self.position(key)
            .and_then(|index| self.targets[index].as_ref())
    }

    /// The slot whose key is `key`, if any — the hot scan.
    ///
    /// It reads `keys` only (16 bytes per slot) over the **occupied prefix**, so a
    /// machine with two distinct local callees scans two slots (one cache line), not
    /// eight. The bound is not a validity argument: `CalleeKey::VACANT` can never be
    /// produced by [`CalleeKey::of`], so an unbounded scan would be just as correct —
    /// bounding it only stops the hot loop from walking vacant slots.
    #[inline(always)]
    fn position(&self, key: CalleeKey) -> Option<usize> {
        self.keys[..self.len]
            .iter()
            .position(|slot| *slot == key)
    }

    /// Store `target` (which may be the "un-nameable" `None`) at the front.
    ///
    /// The cold path: the new entry takes the front slot and the occupied prefix shifts
    /// one place behind it. A full cache shifts the whole array, so the last (oldest)
    /// entry is the one the new front slot overwrites — eviction and insertion are the
    /// same move. A freshly filled entry's verdict is [`Verdict::Ask`] (the host has not
    /// been consulted about it yet), or [`Verdict::Unnameable`] when there is no identity
    /// to ask about.
    fn insert(&mut self, key: CalleeKey, target: Option<CalleeTarget>) {
        let span = if self.len == CAP { CAP } else { self.len + 1 };
        // `keys`/`verdicts` are `Copy`, so the shift is a `copy_within`; `targets` owns
        // the name text, so it is a rotation (the move-to-front equivalent of a shift).
        self.keys[..span].rotate_right(1);
        self.verdicts[..span].rotate_right(1);
        self.targets[..span].rotate_right(1);
        self.keys[0] = key;
        self.verdicts[0] = if target.is_some() {
            Verdict::Ask
        } else {
            Verdict::Unnameable
        };
        self.targets[0] = target;
        if self.len < CAP {
            self.len += 1;
        }
        #[cfg(test)]
        {
            self.fills += 1;
        }
    }
}

impl Default for LocalCallees {
    fn default() -> LocalCallees {
        LocalCallees::new()
    }
}

/// Derive `func`'s identity in `module` — the memo's miss path.
///
/// The same derivation as the seam's cold path
/// ([`local_callee_identity`](crate::interpreter)): the callee's **own** function entry
/// plus the running module's atom table, never the call site's operands and never
/// `module.name`. The difference is only the representation — the memo stores the name
/// text **owned**, so it cannot borrow the module.
fn derive(module: &Module, func: u32) -> Option<CalleeTarget> {
    let function = module.functions.get(func as usize)?;
    let name = module.atom_name(function.name)?;
    let module_name = module.atom_name(function.module)?;
    Some(CalleeTarget {
        module: module_name.into(),
        function: name.into(),
        arity: function.arity,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use beam_loader::{Atom, Function, Label, Module};

    use super::super::test_support::Fixture;
    use super::{LocalCallees, Verdict, CAP};

    /// A module whose `index`-th function is `name/arity`, built through the shared
    /// synthetic-module fixture.
    fn fixture(names: &[&str]) -> Module {
        let mut fixture = Fixture::new("m");
        for name in names {
            fixture.add_function(name, 1, Vec::new());
        }
        fixture.finish()
    }

    /// The identity's `(module, function, arity)` triple.
    fn triple(target: Option<&super::CalleeTarget>) -> Option<(&str, &str, u32)> {
        target.map(|target| {
            (
                target.module.as_ref(),
                target.function.as_ref(),
                target.arity,
            )
        })
    }

    /// The memo's key for `func` in `module` (the tests read the private constructor
    /// through their parent module).
    fn key_of(module: &Module, func: u32) -> super::CalleeKey {
        super::CalleeKey::of(module, func)
    }

    #[test]
    fn a_miss_fills_once_and_the_second_lookup_is_a_hit() {
        let module = fixture(&["a", "b"]);
        let mut cache = LocalCallees::new();

        assert_eq!(
            cache.lookup(&module, 0),
            Verdict::Ask,
            "a miss derives the callee's identity and asks the host about it"
        );
        assert_eq!(triple(cache.target(&module, 0)), Some(("m", "a", 1)));
        assert_eq!(cache.fill_count(), 1, "one derivation");
        assert_eq!(
            cache.lookup(&module, 0),
            Verdict::Ask,
            "the same key answers from the memo (still un-asked)"
        );
        assert_eq!(cache.fill_count(), 1, "a hit derives nothing");
        // Once the host has answered, the hit answers with its verdict.
        cache.record(&module, 0, false);
        assert_eq!(cache.lookup(&module, 0), Verdict::Declined);
        assert_eq!(cache.fill_count(), 1, "no re-derivation on the verdict path");
        assert_eq!(cache.ask_count(), 1, "exactly one host consultation");
    }

    #[test]
    fn a_different_function_fills_again() {
        let module = fixture(&["a", "b"]);
        let mut cache = LocalCallees::new();

        cache.lookup(&module, 0);
        cache.lookup(&module, 1);
        assert_eq!(cache.fill_count(), 2, "each new key derives once");
        cache.lookup(&module, 1);
        assert_eq!(cache.fill_count(), 2, "and is a hit afterwards");
    }

    /// **The policy this memo exists for.** A hit never reorders: an alternating
    /// two-callee loop (what `list_walk`'s inner loop is — `sum/1` then `walk/3`) fills
    /// both callees on its first cycle and is all hits afterwards, with **no** entry
    /// ever moving out of its slot.
    #[test]
    fn a_hit_reorders_nothing_so_an_alternating_pattern_is_all_hits() {
        let module = fixture(&["a", "b"]);
        let mut cache = LocalCallees::new();
        let a = key_of(&module, 0);
        let b = key_of(&module, 1);

        cache.lookup(&module, 0);
        cache.record(&module, 0, false);
        cache.lookup(&module, 1);
        cache.record(&module, 1, false);
        assert_eq!(cache.fill_count(), 2, "one derivation per callee");
        assert_eq!(cache.position(a), Some(1), "the first callee keeps its slot");
        assert_eq!(cache.position(b), Some(0), "the miss inserted at the front");

        for _ in 0..16 {
            assert_eq!(cache.lookup(&module, 0), Verdict::Declined);
            assert_eq!(cache.lookup(&module, 1), Verdict::Declined);
        }
        assert_eq!(cache.fill_count(), 2, "an alternation never re-derives");
        assert_eq!(cache.ask_count(), 2, "and never re-consults the host");
        assert_eq!(cache.position(a), Some(1), "a hit moved nothing");
        assert_eq!(cache.position(b), Some(0), "a hit moved nothing");
    }

    /// The un-nameable answers and the vacant slot's value are distinguishable: a
    /// vacated slot is skipped because its key is `VACANT`, never because its target is
    /// `None`.
    #[test]
    fn a_vacant_slot_is_keyed_out_rather_than_read_as_an_unnameable_callee() {
        let module = fixture(&["a"]);
        let mut cache = LocalCallees::new();

        // Function 7 does not exist: the memoized answer is the *un-nameable* verdict...
        assert_eq!(cache.lookup(&module, 7), Verdict::Unnameable);
        assert_eq!(cache.fill_count(), 1);
        assert_eq!(cache.memoized(key_of(&module, 7)), None);
        // ... and it is a hit, not a re-derivation, on the next call.
        assert_eq!(cache.lookup(&module, 7), Verdict::Unnameable);
        assert_eq!(cache.fill_count(), 1);
        // A cleared cache is empty rather than full of un-nameable entries.
        cache.clear();
        assert_eq!(cache.len, 0);
        assert!(cache.keys.iter().all(|key| *key == super::CalleeKey::VACANT));
        assert!(
            cache
                .verdicts
                .iter()
                .all(|verdict| *verdict == Verdict::Unnameable)
        );
    }

    #[test]
    fn an_out_of_range_function_memoizes_an_unnameable_verdict() {
        let module = fixture(&["a"]);
        let mut cache = LocalCallees::new();

        assert_eq!(
            cache.lookup(&module, 7),
            Verdict::Unnameable,
            "no such function entry"
        );
        assert_eq!(cache.target(&module, 7), None);
        assert_eq!(cache.fill_count(), 1);
        assert_eq!(
            cache.lookup(&module, 7),
            Verdict::Unnameable,
            "the un-nameable verdict is memoized, not re-derived"
        );
        assert_eq!(
            cache.fill_count(),
            1,
            "an un-nameable callee fills once too"
        );
        assert_eq!(cache.ask_count(), 0, "and is never asked about");
    }

    #[test]
    fn a_function_without_a_name_atom_memoizes_an_unnameable_verdict() {
        // A function entry whose name atom the module's atom table does not know: the
        // engine must decline the seam rather than invent a name (the numeric form an
        // atom id could be rendered as is not a name the host may be asked about).
        let mut module = Module::default();
        let name = module.atom_table.intern("m");
        module.name = name;
        module.functions.push(Function {
            func_label: Label(1),
            entry_label: Label(1),
            module: name,
            name: Atom::MAX,
            arity: 2,
            code: Vec::new(),
            blocks: Vec::new(),
            labels: BTreeMap::new(),
        });
        let mut cache = LocalCallees::new();

        assert_eq!(cache.lookup(&module, 0), Verdict::Unnameable);
        assert_eq!(cache.fill_count(), 1);
        assert_eq!(cache.lookup(&module, 0), Verdict::Unnameable);
        assert_eq!(cache.fill_count(), 1, "memoized, never re-derived");
        assert_eq!(cache.ask_count(), 0, "an unnameable callee is never asked about");
    }

    /// Invalidation clears **verdicts only**: every memoized identity survives, so the
    /// next local call re-consults the host and re-derives nothing.
    #[test]
    fn invalidation_re_asks_every_callee_and_keeps_the_identities() {
        let module = fixture(&["a", "b"]);
        let mut cache = LocalCallees::new();

        cache.lookup(&module, 0);
        cache.record(&module, 0, true);
        cache.lookup(&module, 1);
        cache.record(&module, 1, false);
        cache.lookup(&module, 7); // un-nameable
        assert_eq!(cache.fill_count(), 3);
        assert_eq!(cache.ask_count(), 2);

        cache.invalidate_verdicts();
        assert_eq!(
            cache.lookup(&module, 0),
            Verdict::Ask,
            "a served callee must be re-asked"
        );
        assert_eq!(
            cache.lookup(&module, 1),
            Verdict::Ask,
            "a declined callee must be re-asked"
        );
        assert_eq!(
            cache.lookup(&module, 7),
            Verdict::Unnameable,
            "an un-nameable callee stays unnameable: no name to ask about"
        );
        assert_eq!(
            cache.fill_count(),
            3,
            "invalidation re-derives nothing — the identities never go stale"
        );
        // The re-ask records again, and the memo answers with the new verdict.
        cache.record(&module, 0, false);
        assert_eq!(cache.lookup(&module, 0), Verdict::Declined);
        assert_eq!(cache.ask_count(), 3);
    }

    #[test]
    fn clear_empties_the_cache() {
        let module = fixture(&["a"]);
        let mut cache = LocalCallees::new();

        cache.lookup(&module, 0);
        cache.record(&module, 0, true);
        assert_eq!(cache.fill_count(), 1);
        cache.clear();
        assert_eq!(cache.lookup(&module, 0), Verdict::Ask, "a cleared slot re-asks");
        assert_eq!(
            triple(cache.target(&module, 0)),
            Some(("m", "a", 1)),
            "a cleared cache answers correctly again"
        );
        assert_eq!(cache.fill_count(), 2, "the entry was re-derived");
    }

    /// The fill order is the eviction order: only a **miss** inserts (at the front), so
    /// a full cache drops its **oldest insertion** — and, because a hit never promotes,
    /// touching the oldest entry does **not** save it.
    #[test]
    fn a_full_cache_evicts_the_oldest_insertion_and_a_hit_never_promotes() {
        let names: Vec<String> = (0..=CAP).map(|index| format!("f{index}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let module = fixture(&names);
        let mut cache = LocalCallees::new();

        for func in 0..CAP as u32 {
            cache.lookup(&module, func);
        }
        assert_eq!(cache.fill_count(), CAP as u64, "one derivation per slot");
        // Every entry keeps the slot its insertion gave it: the last insertion is at the
        // front, the first at the back.
        assert_eq!(cache.position(key_of(&module, 0)), Some(CAP - 1));
        assert_eq!(cache.position(key_of(&module, CAP as u32 - 1)), Some(0));

        // Reading the oldest entry is a hit and promotes nothing, so it is still the
        // entry the next miss evicts.
        assert_eq!(triple(cache.target(&module, 0)), Some(("m", "f0", 1)));
        assert_eq!(cache.fill_count(), CAP as u64, "a hit derives nothing");

        cache.lookup(&module, CAP as u32);
        assert_eq!(cache.fill_count(), CAP as u64 + 1, "the overflow derives");
        assert_eq!(
            cache.position(key_of(&module, CAP as u32)),
            Some(0),
            "the new entry took the front slot"
        );
        assert_eq!(
            cache.position(key_of(&module, 0)),
            None,
            "the oldest insertion fell off the end, un-promoted by its recent hit"
        );

        // ... and the evicted callee re-derives (and re-asks) on its next call.
        assert_eq!(cache.lookup(&module, 0), Verdict::Ask);
        assert_eq!(cache.fill_count(), CAP as u64 + 2, "it was evicted");
    }

    /// An evicted-and-refilled entry must not inherit the evicted entry's verdict: the
    /// verdict array is shifted with the keys, so the front slot always starts un-asked.
    #[test]
    fn a_refilled_slot_starts_unasked_whatever_it_held_before() {
        let names: Vec<String> = (0..=CAP).map(|index| format!("f{index}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let module = fixture(&names);
        let mut cache = LocalCallees::new();

        for func in 0..CAP as u32 {
            cache.lookup(&module, func);
            cache.record(&module, func, true);
        }
        assert_eq!(cache.ask_count(), CAP as u64);
        // The new entry takes the front slot: un-asked, never `Served` by inheritance.
        assert_eq!(cache.lookup(&module, CAP as u32), Verdict::Ask);
        assert_eq!(
            cache.verdict(key_of(&module, CAP as u32)),
            Some(Verdict::Ask),
            "the refilled front slot starts un-asked"
        );
        // ... and the entry that lost its slot is gone (not left `Served` behind).
        assert_eq!(cache.verdict(key_of(&module, 0)), None);
        assert_eq!(cache.ask_count(), CAP as u64, "no consultation happened yet");
    }
}
