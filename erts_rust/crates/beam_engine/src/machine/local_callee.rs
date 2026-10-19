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
/// enough; the eviction policy (least recently used falls off the end) makes a cold
/// callee merely cost one re-derivation, never a wrong answer.
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
/// address is a sound key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CalleeKey {
    /// The address of the `Module` the function lives in.
    pub module: usize,
    /// The function index ([`Module::functions`] index).
    pub func: u32,
}

impl CalleeKey {
    /// The key of `func` in `module`.
    #[inline]
    fn of(module: &Module, func: u32) -> CalleeKey {
        CalleeKey {
            module: module as *const Module as usize,
            func,
        }
    }

    /// The vacant slot's key: a real module reference is never null, so no key a
    /// `resolve` builds can ever equal this one.
    const VACANT: CalleeKey = CalleeKey { module: 0, func: 0 };
}

/// One slot: its key and its memoized identity (`None` = "un-nameable callee", or a
/// still-vacant slot — the two differ only by their key).
#[derive(Clone, Debug)]
struct Entry {
    key: CalleeKey,
    target: Option<CalleeTarget>,
}

impl Entry {
    /// A slot that no key matches.
    const VACANT: Entry = Entry {
        key: CalleeKey::VACANT,
        target: None,
    };
}

/// The machine's **local-callee identity memo**: a fixed, allocation-free,
/// least-recently-used cache of the identity derivation the local-call seam performs
/// on every local call.
///
/// `entries[..len]` are the occupied slots in most-recently-used-first order, so the
/// callee a loop keeps calling sits at index 0 and a repeat call is one compare. A slot
/// past `len` is [`Entry::VACANT`] (keyed on [`CalleeKey::VACANT`]), which is why
/// [`LocalCallees::resolve`] can answer from the front slot unconditionally — there is
/// no "missing" case left to panic on or to fabricate.
pub(crate) struct LocalCallees {
    /// The slots; `entries[..len]` are occupied.
    entries: [Entry; CAP],
    /// The occupied prefix length (`0..=CAP`).
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
            // `Entry` owns the name text, so it is not `Copy` and the array cannot be
            // built by repetition.
            entries: std::array::from_fn(|_| Entry::VACANT),
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
    /// A hit moves the entry to the front (nothing moves when it is already there, the
    /// steady state of a loop calling one callee); a miss inserts the fresh entry at the
    /// front and lets the least recently used one fall off the end when the cache is
    /// full.
    #[inline]
    pub(crate) fn resolve(&mut self, module: &Module, func: u32) -> &Option<CalleeTarget> {
        let key = CalleeKey::of(module, func);
        match self.position(key) {
            // Already the most recently used: the hot path, two loads and a compare.
            Some(0) => {}
            // Bring it to the front. `Entry` owns the name text, so it is not `Copy` and
            // a rotation is the move-to-front equivalent of a `copy_within` shift.
            Some(index) => self.entries[..=index].rotate_right(1),
            None => self.insert(key, derive(module, func)),
        }
        // Both arms leave `key`'s entry in the front slot, which is all this reads.
        &self.entries[0].target
    }

    /// Drop every memoized entry (`Machine::reset` — the address-keyed entries are only
    /// valid for the run that filled them).
    pub(crate) fn clear(&mut self) {
        for slot in self.entries.iter_mut() {
            *slot = Entry::VACANT;
        }
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
        self.entries[..self.len]
            .iter()
            .find(|entry| entry.key == key)
            .and_then(|entry| entry.target.as_ref())
    }

    /// The occupied index whose key is `key`, if any.
    #[inline]
    fn position(&self, key: CalleeKey) -> Option<usize> {
        self.entries[..self.len]
            .iter()
            .position(|entry| entry.key == key)
    }

    /// Store `target` (which may be the "un-nameable" `None`) at the front.
    fn insert(&mut self, key: CalleeKey, target: Option<CalleeTarget>) {
        // The new entry takes the front slot and the occupied prefix shifts one place
        // behind it. A full cache rotates the whole array, so the last (least recently
        // used) entry is the one the new front slot overwrites — eviction and insertion
        // are the same move.
        let span = if self.len == CAP { CAP } else { self.len + 1 };
        self.entries[..span].rotate_right(1);
        self.entries[0] = Entry { key, target };
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

    #[test]
    fn the_cache_evicts_the_least_recently_used_entry_at_capacity() {
        let names: Vec<String> = (0..=CAP).map(|index| format!("f{index}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let module = fixture(&names);
        let mut cache = LocalCallees::new();

        for func in 0..CAP as u32 {
            cache.resolve(&module, func);
        }
        assert_eq!(cache.fill_count(), CAP as u64, "one derivation per slot");
        // Touching the first entry makes the *second* one the least recently used.
        assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "f0", 1)));
        cache.resolve(&module, CAP as u32);
        assert_eq!(cache.fill_count(), CAP as u64 + 1, "the overflow derives");

        // The newest entry and the re-touched one are memoized...
        assert_eq!(
            triple(cache.resolve(&module, CAP as u32)),
            Some(("m", names[CAP], 1))
        );
        assert_eq!(triple(cache.resolve(&module, 0)), Some(("m", "f0", 1)));
        assert_eq!(cache.fill_count(), CAP as u64 + 1, "both are hits");

        // ... while the least recently used one (index 1) had to be re-derived.
        assert_eq!(triple(cache.resolve(&module, 1)), Some(("m", "f1", 1)));
        assert_eq!(cache.fill_count(), CAP as u64 + 2, "it was evicted");
    }
}
