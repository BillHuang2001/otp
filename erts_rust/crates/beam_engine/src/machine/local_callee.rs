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

//! The **local-callee identity memo** behind [`Machine::local_callees`].
//!
//! [`Machine::local_call_seam`](crate::Machine) has to name the same-module callee of
//! every local `call`/`call_only`/`call_last` — its `(module name, function name,
//! arity)` — before it can ask the host
//! [`ExecutionContext::resolve_local_call`](crate::ExecutionContext::resolve_local_call).
//! That derivation reads the callee's **own** function entry
//! ([`Module::functions`]) and two of the running module's atom names, and all three
//! values are **immutable for a loaded module**: the function table and the atom table
//! are built once at load time and never mutated afterwards. The derivation is
//! therefore memoizable per `(module, function index)`, which is what this cache does.
//!
//! **Only the derivation is memoized — never the host's verdict.** The seam consults
//! `resolve_local_call` exactly once per local call (a host spends one reduction unit
//! per consultation, and `erlang:load_nif/2` can install a stub that a later call must
//! be served from), while the three-value identity is derived once per
//! `(module, function index)` and read from here afterwards. An **un-nameable** callee
//! is memoized as `None` (the inner `Option`) and is then consulted **zero** times —
//! exactly as before the cache existed, since the seam never asks the host about a
//! fabricated name.
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
//! The storage is split into two parallel arrays — a compact `keys: [CalleeKey; CAP]`
//! (16 bytes per entry, so the whole key array is two cache lines) and the
//! `targets: [Option<CalleeTarget>; CAP]` payloads, which own their name text. The scan
//! therefore reads keys only and the payload of a slot is touched solely on a hit. The
//! scan covers the whole fixed array with no length bound: a vacant slot is keyed on
//! [`CalleeKey::VACANT`], which [`CalleeKey::of`] can never produce, so no match can
//! come from past `len` and the loop is a fixed eight-iteration test.
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

/// The number of memoized callee identities one machine keeps.
///
/// A machine executes one module at a time and a module's local calls reach a handful
/// of distinct callees in any one workload, so a fixed, allocation-free array is
/// enough; the eviction policy (the **oldest insertion** falls off the end) makes a
/// cold callee merely cost one re-derivation, never a wrong answer.
const CAP: usize = 8;

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

/// The machine's **local-callee identity memo**: a fixed, allocation-free cache of the
/// identity derivation the local-call seam performs on every local call.
///
/// `keys[..len]` and `targets[..len]` are the occupied slots, front (most recent
/// insertion) first; a slot past `len` is vacant (`keys` = [`CalleeKey::VACANT`],
/// `targets` = `None`). The split is what keeps the hot scan on the keys: the payloads
/// own their name text and are touched only once a key has matched.
pub(crate) struct LocalCallees {
    /// The slot keys — the whole hot scan reads only this array.
    keys: [CalleeKey; CAP],
    /// The memoized identities, parallel to `keys`.
    targets: [Option<CalleeTarget>; CAP],
    /// The occupied prefix length (`0..=CAP`); bookkeeping for the insert shift and for
    /// the test-only accessors (the scan itself needs no bound — see the module docs).
    len: usize,
    /// Test-only: how many times an identity was **derived** (a miss), so a test can
    /// prove the memoization count deterministically. Absent from non-test builds.
    #[cfg(test)]
    fills: u64,
}

impl LocalCallees {
    /// An empty cache.
    pub(crate) fn new() -> LocalCallees {
        LocalCallees {
            keys: [CalleeKey::VACANT; CAP],
            targets: std::array::from_fn(|_| None),
            len: 0,
            #[cfg(test)]
            fills: 0,
        }
    }

    /// The memoized identity of `func` in `module`, deriving it on a miss.
    ///
    /// The returned option is the memo's own answer: `Some(target)` names the callee,
    /// `None` records that the callee is **un-nameable** (no function entry, or a name
    /// atom the module cannot resolve), which the seam treats exactly as it always
    /// has — by declining without consulting the host.
    ///
    /// **A hit reorders nothing** (the entry keeps its slot, which is what makes an
    /// alternating-callee loop all hits after its first cycle); **a miss** derives once
    /// and inserts at the front, letting the oldest insertion fall off the end when the
    /// cache is full. `#[inline(always)]`: the body is one fixed scan, so a call that
    /// stayed out of line would cost more than the derivation it saves.
    #[inline(always)]
    pub(crate) fn resolve(&mut self, module: &Module, func: u32) -> &Option<CalleeTarget> {
        let key = CalleeKey::of(module, func);
        // A hit is a read: the slot the key already occupies answers, and nothing moves.
        if let Some(index) = self.position(key) {
            return &self.targets[index];
        }
        self.insert(key, derive(module, func));
        &self.targets[0]
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
        self.len = 0;
    }

    /// The number of identity derivations this machine has performed (test-only).
    #[cfg(test)]
    pub(crate) fn fill_count(&self) -> u64 {
        self.fills
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
    /// It reads `keys` only (16 bytes per slot) and needs no length bound: a vacant
    /// slot's key is [`CalleeKey::VACANT`], which no [`CalleeKey::of`] produces.
    #[inline(always)]
    fn position(&self, key: CalleeKey) -> Option<usize> {
        self.keys.iter().position(|slot| *slot == key)
    }

    /// Store `target` (which may be the "un-nameable" `None`) at the front.
    ///
    /// The cold path: the new entry takes the front slot and the occupied prefix shifts
    /// one place behind it. A full cache shifts the whole array, so the last (oldest)
    /// entry is the one the new front slot overwrites — eviction and insertion are the
    /// same move.
    fn insert(&mut self, key: CalleeKey, target: Option<CalleeTarget>) {
        let span = if self.len == CAP { CAP } else { self.len + 1 };
        // `keys` is `Copy`, so the shift is a `copy_within`; `targets` owns the name
        // text, so it is a rotation (the move-to-front equivalent of a shift).
        self.keys[..span].rotate_right(1);
        self.targets[..span].rotate_right(1);
        self.keys[0] = key;
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
    use super::{LocalCallees, CAP};

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
    fn triple(target: &Option<super::CalleeTarget>) -> Option<(&str, &str, u32)> {
        target.as_ref().map(|target| {
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
    fn a_miss_fills_once_and_the_second_resolve_is_a_hit() {
        let module = fixture(&["a", "b"]);
        let mut cache = LocalCallees::new();

        assert_eq!(
            triple(cache.resolve(&module, 0)),
            Some(("m", "a", 1)),
            "a miss derives the callee's own identity"
        );
        assert_eq!(cache.fill_count(), 1, "one derivation");
        assert_eq!(
            triple(cache.resolve(&module, 0)),
            Some(("m", "a", 1)),
            "the same key answers from the memo"
        );
        assert_eq!(cache.fill_count(), 1, "a hit derives nothing");
    }

    #[test]
    fn a_different_function_fills_again() {
        let module = fixture(&["a", "b"]);
        let mut cache = LocalCallees::new();

        assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "a", 1)));
        assert_eq!(triple(cache.resolve(&module, 1)), Some(("m", "b", 1)));
        assert_eq!(cache.fill_count(), 2, "each new key derives once");
        assert_eq!(triple(cache.resolve(&module, 1)), Some(("m", "b", 1)));
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

        assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "a", 1)));
        assert_eq!(triple(cache.resolve(&module, 1)), Some(("m", "b", 1)));
        assert_eq!(cache.fill_count(), 2, "one derivation per callee");
        assert_eq!(cache.position(a), Some(1), "the first callee keeps its slot");
        assert_eq!(cache.position(b), Some(0), "the miss inserted at the front");

        for _ in 0..16 {
            assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "a", 1)));
            assert_eq!(triple(cache.resolve(&module, 1)), Some(("m", "b", 1)));
        }
        assert_eq!(cache.fill_count(), 2, "an alternation never re-derives");
        assert_eq!(cache.position(a), Some(1), "a hit moved nothing");
        assert_eq!(cache.position(b), Some(0), "a hit moved nothing");
    }

    /// The un-nameable answers and the `None` of a vacant slot are distinguishable: a
    /// vacated slot is skipped because its key is `VACANT`, never because its target is
    /// `None`.
    #[test]
    fn a_vacant_slot_is_keyed_out_rather_than_read_as_an_unnameable_callee() {
        let module = fixture(&["a"]);
        let mut cache = LocalCallees::new();

        // Function 7 does not exist: the memoized answer is the *un-nameable* `None`...
        assert_eq!(triple(cache.resolve(&module, 7)), None);
        assert_eq!(cache.fill_count(), 1);
        assert_eq!(cache.memoized(key_of(&module, 7)), None);
        // ... and it is a hit, not a re-derivation, on the next call.
        assert_eq!(triple(cache.resolve(&module, 7)), None);
        assert_eq!(cache.fill_count(), 1);
        // A cleared cache is empty rather than full of un-nameable entries.
        cache.clear();
        assert_eq!(cache.len, 0);
        assert!(cache.keys.iter().all(|key| *key == super::CalleeKey::VACANT));
    }

    #[test]
    fn an_out_of_range_function_memoizes_none() {
        let module = fixture(&["a"]);
        let mut cache = LocalCallees::new();

        assert_eq!(cache.resolve(&module, 7), &None, "no such function entry");
        assert_eq!(cache.fill_count(), 1);
        assert_eq!(
            cache.resolve(&module, 7),
            &None,
            "the un-nameable verdict is memoized, not re-derived"
        );
        assert_eq!(
            cache.fill_count(),
            1,
            "an un-nameable callee fills once too"
        );
    }

    #[test]
    fn a_function_without_a_name_atom_memoizes_none() {
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

        assert_eq!(cache.resolve(&module, 0), &None);
        assert_eq!(cache.fill_count(), 1);
        assert_eq!(cache.resolve(&module, 0), &None);
        assert_eq!(cache.fill_count(), 1, "memoized, never re-derived");
    }

    #[test]
    fn clear_empties_the_cache() {
        let module = fixture(&["a"]);
        let mut cache = LocalCallees::new();

        cache.resolve(&module, 0);
        assert_eq!(cache.fill_count(), 1);
        cache.clear();
        assert_eq!(
            cache.resolve(&module, 0),
            &Some(super::CalleeTarget {
                module: "m".into(),
                function: "a".into(),
                arity: 1
            }),
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
            cache.resolve(&module, func);
        }
        assert_eq!(cache.fill_count(), CAP as u64, "one derivation per slot");
        // Every entry keeps the slot its insertion gave it: the last insertion is at the
        // front, the first at the back.
        assert_eq!(cache.position(key_of(&module, 0)), Some(CAP - 1));
        assert_eq!(cache.position(key_of(&module, CAP as u32 - 1)), Some(0));

        // Reading the oldest entry is a hit and promotes nothing, so it is still the
        // entry the next miss evicts.
        assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "f0", 1)));
        assert_eq!(cache.fill_count(), CAP as u64, "a hit derives nothing");

        cache.resolve(&module, CAP as u32);
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

        // ... and the evicted callee re-derives once on its next call.
        assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "f0", 1)));
        assert_eq!(cache.fill_count(), CAP as u64 + 2, "it was evicted");
    }
}
