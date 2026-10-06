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

/// Maps `(module, function, arity)` to a [`BifFn`].
#[derive(Default)]
pub struct BifRegistry {
    map: HashMap<(String, String, u32), BifFn>,
}

impl BifRegistry {
    /// An empty registry.
    pub fn new() -> BifRegistry {
        BifRegistry {
            map: HashMap::new(),
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
        crate::reason::register(&mut reg);
        crate::string::register(&mut reg);
        crate::io::register(&mut reg);
        crate::ets::register(&mut reg);
        crate::port::register(&mut reg);
        reg
    }

    /// Registers `f` under `(module, function, arity)`, returning the previous
    /// implementation if that key was already present.
    pub fn register(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        f: BifFn,
    ) -> Option<BifFn> {
        self.map
            .insert((module.to_string(), function.to_string(), arity), f)
    }

    /// The implementation registered under `(module, function, arity)`, if any.
    pub fn lookup(&self, module: &str, function: &str, arity: u32) -> Option<BifFn> {
        self.map
            .get(&(module.to_string(), function.to_string(), arity))
            .copied()
    }

    /// Whether `(module, function, arity)` is registered.
    pub fn contains(&self, module: &str, function: &str, arity: u32) -> bool {
        self.map
            .contains_key(&(module.to_string(), function.to_string(), arity))
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
        self.map.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every registered `(module, function, arity)` key, sorted.
    pub fn names(&self) -> Vec<(String, String, u32)> {
        let mut names: Vec<(String, String, u32)> = self.map.keys().cloned().collect();
        names.sort();
        names
    }

    /// Every registered entry as `(module, function, arity, impl)`.
    pub fn entries(&self) -> impl Iterator<Item = (String, String, u32, BifFn)> + '_ {
        self.map
            .iter()
            .map(|((module, function, arity), f)| (module.clone(), function.clone(), *arity, *f))
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
    fn default_registry_is_sorted_and_matches_with_core_bifs() {
        let dflt = default_registry();
        assert_eq!(dflt.len(), BifRegistry::with_core_bifs().len());
        // `default` is `new`.
        assert_eq!(BifRegistry::default().names(), BifRegistry::new().names());
        // `names()` is sorted by (module, function, arity).
        let names = dflt.names();
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
    }
}
