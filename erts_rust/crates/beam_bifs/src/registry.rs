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

//! The BIF registry: `(module, function, arity) → implementation`.
//!
//! The registry is the seam `beam_rt` adapts into the engine's
//! `ExecutionContext::call_bif` (see `../beam_engine/CONTEXT.md`). The key is the
//! `(module, function, arity)` triple exactly as the loader resolves it from a
//! module's `imports` — and exactly the shape compatibility contract Part I §A.7
//! pins. Operator BIFs keep their *quoted* names (`"+"`, `"=<"`, `"=:="`, `"!"`, …),
//! matching `../../erts/emulator/beam/bif.tab`.
//!
//! [`BifFn`] is a plain `fn` pointer, not a boxed closure: the registry is built
//! once at boot and looked up per BIF call, and every piece of BIF state flows
//! through [`BifContext`] (see the crate `CONTEXT.md`, Design Decisions).
//!
//! # Why the lookup is allocation-free
//!
//! [`BifRegistry::lookup`] and [`BifRegistry::contains`] run on **every** registered
//! BIF dispatch (a real dispatch does `contains` and then `call` → `lookup`), so they
//! must not touch the heap. A flat `HashMap<(String, String, u32), BifFn>` cannot do
//! that: a `(String, String, u32)` key has to be *owned*, so each lookup built two
//! `String`s (`module.to_string()` + `function.to_string()`) before it could hash —
//! four allocations per dispatch.
//!
//! The internals are therefore **nested maps shadowed by borrows**:
//!
//! ```text
//! module: String ─▶ function: String ─▶ [(arity, BifFn), …]
//! ```
//!
//! `HashMap<String, _>::get` accepts a `&str` argument through `Borrow<str>`, and
//! `String`'s `Hash`/`Eq` delegate to `str`'s, so hashing a borrowed `&str` is
//! equivalent to hashing the owned key — **no key is ever built**. The innermost
//! level is a small `Vec` scanned linearly: a BIF function has one arity in almost
//! every case (`erlang:error/1..3` is the widest in the roster), so the scan touches
//! one or two elements, needs no hashing, and — being keyed only by `u32` — needs no
//! allocation either. A miss stops at the first `get` that returns `None`.
//!
//! Registration is boot-time only, so it may allocate (it interns the module/function
//! names once). `names()`/`entries()`/`len()` keep their exact semantics; `len()` is a
//! cached counter so it stays O(1).

use std::collections::HashMap;

use beam_common::Result;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::error;

/// A BIF implementation.
///
/// The registry guarantees `args.len()` equals the registered arity before
/// invoking it.
pub type BifFn = fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>;

/// One function's `(arity, impl)` pairs, in registration order.
///
/// A linear scan over this `Vec` replaces a `HashMap<u32, BifFn>`: the vectors are
/// tiny (one element for nearly every function), so the scan wins on both cache
/// locality and allocation, and a keyed-by-`u32` map would add nothing.
type ArityTable = Vec<(u32, BifFn)>;

/// `function → arities`.
type FunctionTable = HashMap<String, ArityTable>;

/// `module → functions`.
type ModuleTable = HashMap<String, FunctionTable>;

/// Maps `(module, function, arity)` to a [`BifFn`].
///
/// The nested shape makes `lookup`/`contains` allocation-free (see the module docs).
#[derive(Default)]
pub struct BifRegistry {
    /// `module → function → [(arity, impl)]`.
    map: ModuleTable,
    /// The number of registered `(module, function, arity)` keys, cached so `len()`
    /// is O(1) without walking the nested maps.
    len: usize,
}

impl BifRegistry {
    /// An empty registry.
    pub fn new() -> BifRegistry {
        BifRegistry {
            map: HashMap::new(),
            len: 0,
        }
    }

    /// The Milestone-1 core registry: every BIF in the crate `CONTEXT.md`
    /// "implemented BIF list" (registered by the per-module `register` functions).
    pub fn with_core_bifs() -> BifRegistry {
        let mut reg = BifRegistry::new();
        crate::arith::register(&mut reg);
        crate::bool::register(&mut reg);
        crate::compare::register(&mut reg);
        crate::type_test::register(&mut reg);
        crate::term_ops::register(&mut reg);
        crate::convert::register(&mut reg);
        crate::lists::register(&mut reg);
        crate::maps::register(&mut reg);
        crate::math::register(&mut reg);
        crate::binary::register(&mut reg);
        crate::binconv::register(&mut reg);
        crate::unicode::register(&mut reg);
        crate::time::register(&mut reg);
        crate::phash::register(&mut reg);
        crate::proc::register(&mut reg);
        crate::dict::register(&mut reg);
        crate::reason::register(&mut reg);
        crate::string::register(&mut reg);
        crate::io::register(&mut reg);
        crate::ets::register(&mut reg);
        crate::port::register(&mut reg);
        crate::records::register(&mut reg);
        crate::nif::register(&mut reg);
        crate::checksum::register(&mut reg);
        crate::calendar::register(&mut reg);
        crate::os_bif::register(&mut reg);
        crate::fun_info::register(&mut reg);
        crate::term_conv::register(&mut reg);
        crate::erts_internal::register(&mut reg);
        crate::modinfo::register(&mut reg);
        crate::match_spec::register(&mut reg);
        crate::packet::register(&mut reg);
        crate::proc_reg::register(&mut reg);
        crate::persistent_term::register(&mut reg);
        crate::runtime::register(&mut reg);
        reg
    }

    /// Registers `f` under `(module, function, arity)`, returning the previous
    /// implementation if that key was already present.
    ///
    /// Boot-time only: this is the one operation that allocates (it interns the
    /// module/function names on first sight).
    pub fn register(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        f: BifFn,
    ) -> Option<BifFn> {
        let functions = self.map.entry(module.to_string()).or_default();
        let arities = functions.entry(function.to_string()).or_default();
        if let Some(slot) = arities.iter_mut().find(|(a, _)| *a == arity) {
            return Some(std::mem::replace(&mut slot.1, f));
        }
        arities.push((arity, f));
        self.len += 1;
        None
    }

    /// The implementation registered under `(module, function, arity)`, if any.
    ///
    /// Allocation-free on both a hit and a miss: `module`/`function` are hashed as
    /// borrowed `&str`s and the arity is matched by a linear scan of a tiny `Vec`.
    pub fn lookup(&self, module: &str, function: &str, arity: u32) -> Option<BifFn> {
        self.map
            .get(module)?
            .get(function)?
            .iter()
            .find(|(a, _)| *a == arity)
            .map(|(_, f)| *f)
    }

    /// Whether `(module, function, arity)` is registered.
    ///
    /// Allocation-free (it is [`lookup`](Self::lookup) with the implementation
    /// discarded).
    pub fn contains(&self, module: &str, function: &str, arity: u32) -> bool {
        self.lookup(module, function, arity).is_some()
    }

    /// Looks up `(module, function, arity)`, checks the argument count and invokes
    /// the implementation.
    ///
    /// # Errors
    ///
    /// An unknown key is [`error::undefined_bif`] (an `undef` raise);
    /// `args.len() != arity` is [`error::badarity`]. Otherwise the implementation's
    /// own error is propagated.
    pub fn call(
        &self,
        module: &str,
        function: &str,
        arity: u32,
        ctx: &mut BifContext<'_>,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match self.lookup(module, function, arity) {
            None => Err(error::undefined_bif()),
            Some(f) => {
                if args.len() != arity as usize {
                    return Err(error::badarity());
                }
                f(ctx, args)
            }
        }
    }

    /// The number of registered BIFs.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every registered `(module, function, arity)` key, sorted.
    pub fn names(&self) -> Vec<(String, String, u32)> {
        let mut names: Vec<(String, String, u32)> = Vec::with_capacity(self.len);
        for (module, functions) in &self.map {
            for (function, arities) in functions {
                for &(arity, _) in arities {
                    names.push((module.clone(), function.clone(), arity));
                }
            }
        }
        names.sort();
        names
    }

    /// Every registered entry as `(module, function, arity, impl)`.
    pub fn entries(&self) -> impl Iterator<Item = (String, String, u32, BifFn)> + '_ {
        self.map.iter().flat_map(|(module, functions)| {
            functions.iter().flat_map(move |(function, arities)| {
                arities
                    .iter()
                    .map(move |&(arity, f)| (module.clone(), function.clone(), arity, f))
            })
        })
    }
}

/// The registry with the Milestone-1 core BIFs registered.
pub fn default_registry() -> BifRegistry {
    BifRegistry::with_core_bifs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    /// A trivial BIF returning the small integer `7`.
    fn seven(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
        Ok(make_small(7))
    }

    /// A trivial BIF returning the small integer `8` (a distinguishable duplicate).
    fn eight(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
        Ok(make_small(8))
    }

    fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    #[test]
    fn register_lookup_and_contains() {
        let mut reg = BifRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);

        assert!(reg.register("erlang", "seven", 0, seven).is_none());
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("erlang", "seven", 0));
        assert!(reg.lookup("erlang", "seven", 0).is_some());
        // A different arity or name is a miss.
        assert!(reg.lookup("erlang", "seven", 1).is_none());
        assert!(reg.lookup("erlang", "eight", 0).is_none());
        assert!(!reg.contains("erlang", "seven", 2));
    }

    #[test]
    fn registering_twice_returns_the_previous_impl() {
        let mut reg = BifRegistry::new();
        assert!(reg.register("m", "f", 1, seven).is_none());
        assert!(reg.register("m", "f", 1, seven).is_some());
        assert_eq!(reg.len(), 1, "the second registration replaces the first");
    }

    #[test]
    fn arities_of_one_function_are_independent() {
        let mut reg = BifRegistry::new();
        assert!(reg.register("m", "f", 0, seven).is_none());
        assert!(reg.register("m", "f", 1, eight).is_none());
        assert_eq!(reg.len(), 2, "two arities are two keys");
        assert!(reg.lookup("m", "f", 0).is_some());
        assert!(reg.lookup("m", "f", 1).is_some());

        let mut heap = Heap::new(16).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = context(&mut heap, &atoms);
        assert_eq!(reg.call("m", "f", 0, &mut ctx, &[]).expect("f/0"), make_small(7));
        assert_eq!(
            reg.call("m", "f", 1, &mut ctx, &[make_small(0)]).expect("f/1"),
            make_small(8)
        );

        // Re-registering *one* arity leaves the other untouched.
        assert!(reg.register("m", "f", 0, eight).is_some());
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.call("m", "f", 0, &mut ctx, &[]).expect("f/0 replaced"),
            make_small(8)
        );
        assert_eq!(
            reg.call("m", "f", 1, &mut ctx, &[make_small(0)]).expect("f/1 untouched"),
            make_small(8)
        );

        // An unregistered arity of a registered function is a miss.
        assert!(reg.lookup("m", "f", 2).is_none());
        assert!(!reg.contains("m", "f", 2));
    }

    #[test]
    fn same_function_name_in_different_modules_is_independent() {
        let mut reg = BifRegistry::new();
        reg.register("a", "f", 0, seven);
        reg.register("b", "f", 0, eight);
        assert_eq!(reg.len(), 2);

        let mut heap = Heap::new(16).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = context(&mut heap, &atoms);
        assert_eq!(reg.call("a", "f", 0, &mut ctx, &[]).expect("a:f/0"), make_small(7));
        assert_eq!(reg.call("b", "f", 0, &mut ctx, &[]).expect("b:f/0"), make_small(8));
        assert!(reg.lookup("c", "f", 0).is_none());
    }

    #[test]
    fn contains_always_agrees_with_lookup() {
        let reg = default_registry();
        for (module, function, arity) in reg.names().iter().take(40) {
            assert!(reg.contains(module, function, *arity));
            assert!(reg.lookup(module, function, *arity).is_some());
        }
        assert_eq!(
            reg.contains("erlang", "no_such_bif", 3),
            reg.lookup("erlang", "no_such_bif", 3).is_some()
        );
    }

    #[test]
    fn call_dispatches_and_reports_failures() {
        let mut reg = BifRegistry::new();
        reg.register("erlang", "seven", 0, seven);

        let mut heap = Heap::new(16).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = context(&mut heap, &atoms);

        assert_eq!(
            reg.call("erlang", "seven", 0, &mut ctx, &[])
                .expect("a registered BIF runs"),
            make_small(7)
        );

        // A wrong argument count is `badarity`.
        assert!(matches!(
            reg.call("erlang", "seven", 0, &mut ctx, &[make_small(1)]),
            Err(BeamError::BadArity)
        ));

        // An unknown key is an `undef` raise.
        match reg.call("m", "f", 3, &mut ctx, &[]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, beam_common::ExitReason::atom("undef"));
            }
            other => panic!("expected an undef raise, got {other:?}"),
        }
    }

    #[test]
    fn names_and_entries_are_complete_and_sorted() {
        let mut reg = BifRegistry::new();
        reg.register("m", "b", 1, seven);
        reg.register("m", "a", 2, seven);
        assert_eq!(
            reg.names(),
            vec![
                ("m".to_string(), "a".to_string(), 2),
                ("m".to_string(), "b".to_string(), 1),
            ]
        );
        assert_eq!(reg.entries().count(), 2);
    }

    #[test]
    fn entries_yields_exactly_the_registered_keys() {
        let mut reg = BifRegistry::new();
        reg.register("z", "f", 3, seven);
        reg.register("a", "g", 1, eight);
        reg.register("a", "f", 2, seven);
        reg.register("z", "f", 1, eight);

        let mut from_entries: Vec<(String, String, u32)> = reg
            .entries()
            .map(|(module, function, arity, _)| (module, function, arity))
            .collect();
        from_entries.sort();
        assert_eq!(from_entries, reg.names());
        assert_eq!(reg.len(), 4);
        assert_eq!(reg.entries().count(), reg.len());
    }

    #[test]
    fn default_registry_is_sorted_and_matches_with_core_bifs() {
        let dflt = default_registry();
        assert_eq!(dflt.len(), BifRegistry::with_core_bifs().len());
        // `default` is `new`.
        assert_eq!(BifRegistry::default().names(), BifRegistry::new().names());
        // `names()` is sorted by (module, function, arity).
        let names = dflt.names();
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
        // The nested map agrees with the cached key count.
        assert_eq!(names.len(), dflt.len());
        assert_eq!(dflt.entries().count(), dflt.len());
    }
}
