//! The runtime seam: everything the engine needs from its host.
//!
//! [`ExecutionContext`] isolates BIF dispatch (`call_bif`) and heap management
//! (`ensure_heap`) behind a trait, so the Stage-1 engine depends only on
//! `beam_loader` and never on the Stage-5 runtime core (`../CONTEXT.md`, "The seam
//! types"). `beam_rt` supplies the concrete implementation; [`MinimalContext`] is
//! the test/embedding implementation (unbounded heap plus a pluggable BIF hook).
//!
//! The same trait also exposes the hooks the term-building layer
//! ([`crate::terms`]) and the call paths need: [`alloc_words`] hands out a stable
//! pointer to raw heap words, [`apply`] is the general external-function dispatch
//! entry point, and [`apply_fun`] applies a fun/closure value (the `call_fun`/
//! `apply` path). All are added with default bodies, so an existing implementor
//! keeps compiling unchanged.
//!
//! [`alloc_words`]: ExecutionContext::alloc_words
//! [`apply`]: ExecutionContext::apply
//! [`apply_fun`]: ExecutionContext::apply_fun

use beam_common::Result;
use beam_term::Eterm;

/// Everything the engine needs from the runtime (BIF dispatch, heap/GC, limits).
pub trait ExecutionContext {
    /// Dispatch `module:function/arity` with `args`, returning the result term.
    ///
    /// Failure is return-based: an `Err` (a `BadArith`/`BadArg`/`Raise`/…) that the
    /// engine turns into a guard branch or a raise (it never panics).
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm>;

    /// Ensure `heap_need` heap words are available (for `test_heap`/
    /// `allocate_heap`).
    ///
    /// The live root set is `x_regs[0..live)` plus the whole `stack`. A real context
    /// (`beam_rt`, Stages 4/5) runs the collector here and may **move terms**,
    /// updating `x_regs` and `stack` in place — there are no frame liveness maps.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()>;

    /// Heap words currently available (introspection). Default: unbounded.
    fn heap_available(&self) -> usize {
        usize::MAX
    }

    /// Allocate `n` **uninitialized** heap words and return a pointer to them.
    ///
    /// The caller (`crate::terms`) may write boxed terms into the returned block
    /// and read them back. The pointer is valid **until the next
    /// [`ensure_heap`](ExecutionContext::ensure_heap) call / GC**, which may move
    /// or reclaim the block; it must never be retained across such a call. This
    /// mirrors the C heap-pointer convention (`HAlloc`/`heap_top`): the heap is
    /// owned by the context and the engine only borrows raw storage.
    ///
    /// The default body is a typed [`crate::error::unsupported`]: a context with
    /// no exposed heap (only `call_bif`/`ensure_heap`) cannot serve raw words.
    /// [`MinimalContext`] and the production `beam_rt` context override it.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        let _ = n;
        Err(crate::error::unsupported("alloc_words"))
    }

    /// Dispatch the external-function / BIF application `module:function/arity`.
    ///
    /// This is the general `apply`/`call_ext` hook; the default delegates to
    /// [`call_bif`](ExecutionContext::call_bif) (the BIF dispatch path), so a
    /// context that only implements the BIF surface needs no extra work.
    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        self.call_bif(module, function, arity, args)
    }

    /// Apply a fun/closure value to `args` (the `call_fun`/`apply` path).
    ///
    /// A host with a module/closure registry overrides this to look the fun's
    /// target up and run it. The default body is a typed
    /// [`crate::error::unsupported`]: the engine keeps no closure registry, so a
    /// context that does not override this cannot apply funs.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let _ = (fun, args);
        Err(crate::error::unsupported("apply_fun"))
    }
}

/// A BIF hook: maps `(module, function, arity, args)` to a result.
pub type BifHook = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>;

/// An apply hook: computes the result of applying a fun/closure value to `args`.
///
/// This is the pluggable body of [`ExecutionContext::apply_fun`]: the `call_fun`/
/// `call_fun2` opcodes reach the host through it. A host that keeps a closure
/// registry (or, as in the tests, knows the closures a module builds) overrides it;
/// [`MinimalContext::new`] leaves it unset, so `apply_fun` stays a typed
/// `unsupported`.
pub type ApplyHook = Box<dyn FnMut(Eterm, &[Eterm]) -> Result<Eterm>>;

/// Minimal context: an unbounded heap plus a pluggable BIF hook.
///
/// Used by tests and embeddings. [`MinimalContext::ensure_heap`] is a no-op (the
/// heap is treated as unbounded), [`MinimalContext::new`] installs a default hook
/// that rejects every BIF with [`crate::error::unsupported`], and
/// [`MinimalContext::alloc_words`](ExecutionContext::alloc_words) hands out words
/// from a pointer-stable arena (see the field docs).
///
/// An **apply hook** ([`with_apply_hook`](MinimalContext::with_apply_hook) /
/// [`set_apply_hook`](MinimalContext::set_apply_hook)) may additionally be installed
/// to serve [`apply_fun`](ExecutionContext::apply_fun); without one it is a typed
/// `unsupported`.
pub struct MinimalContext {
    hook: BifHook,
    /// An optional fun-application hook (the [`ExecutionContext::apply_fun`] body).
    apply_hook: Option<ApplyHook>,
    /// A pointer-stable arena backing [`ExecutionContext::alloc_words`].
    ///
    /// Each allocation is its own `Box<[Eterm]>`: growing the outer `Vec` moves
    /// the *box handles*, never the boxes' buffers, so a pointer handed out by
    /// `alloc_words` stays valid for the lifetime of the context (in this context
    /// `ensure_heap` never moves anything). The boxes are never freed before the
    /// context is dropped, which is exactly the "valid until the next
    /// `ensure_heap`" stability contract the trait documents.
    arena: Vec<Box<[Eterm]>>,
}

impl MinimalContext {
    /// A context whose hook rejects every BIF with a typed unsupported error.
    pub fn new() -> MinimalContext {
        MinimalContext::with_bif_hook(Box::new(|module, function, _arity, _args| {
            Err(crate::error::unsupported(&format!(
                "bif {module}:{function}"
            )))
        }))
    }

    /// A context with the given BIF hook.
    pub fn with_bif_hook(hook: BifHook) -> MinimalContext {
        MinimalContext {
            hook,
            apply_hook: None,
            arena: Vec::new(),
        }
    }

    /// A context with the given fun-application hook and the default (rejecting)
    /// BIF hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_apply_hook(hook: ApplyHook) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.apply_hook = Some(hook);
        ctx
    }

    /// Replace the BIF hook.
    pub fn set_bif_hook(&mut self, hook: BifHook) {
        self.hook = hook;
    }

    /// Install (or replace) the fun-application hook.
    pub fn set_apply_hook(&mut self, hook: ApplyHook) {
        self.apply_hook = Some(hook);
    }
}

impl Default for MinimalContext {
    /// Same as [`MinimalContext::new`]: the rejecting default hook.
    fn default() -> MinimalContext {
        MinimalContext::new()
    }
}

impl ExecutionContext for MinimalContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        (self.hook)(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        // Unbounded heap: nothing to do.
        Ok(())
    }

    fn heap_available(&self) -> usize {
        usize::MAX
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // `vec![..; n]` initialises every word; a zero-length box still yields a
        // dangling-but-non-null, well-aligned pointer (the documented result of a
        // zero-word request). Taking `as_mut_ptr` after boxing is safe Rust.
        let mut block = vec![Eterm::default(); n].into_boxed_slice();
        let ptr = block.as_mut_ptr();
        // Keep the buffer alive and immovable for the lifetime of the context.
        self.arena.push(block);
        Ok(ptr)
    }

    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        // No separate external-call machinery in the minimal context: the BIF hook
        // is the dispatch path.
        self.call_bif(module, function, arity, args)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        // Without an installed apply hook there is no closure registry to consult,
        // so fun application stays the trait's documented typed `unsupported`.
        match self.apply_hook.as_mut() {
            Some(hook) => hook(fun, args),
            None => {
                let _ = (fun, args);
                Err(crate::error::unsupported("apply_fun"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hook_rejects_bifs() {
        let mut ctx = MinimalContext::new();
        assert!(ctx.call_bif("erlang", "'+'", 2, &[]).is_err());
    }

    #[test]
    fn custom_hook_dispatches() {
        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, _args| {
            assert_eq!((m, f, a), ("erlang", "'+'", 2));
            Ok(Eterm::nil())
        }));
        assert_eq!(ctx.call_bif("erlang", "'+'", 2, &[]).unwrap(), Eterm::nil());
    }

    #[test]
    fn heap_is_unbounded_and_ensure_is_a_noop() {
        let mut ctx = MinimalContext::default();
        assert_eq!(ctx.heap_available(), usize::MAX);
        let mut x = [Eterm::nil()];
        let mut stack = [Eterm::nil()];
        assert!(ctx.ensure_heap(1_000_000, 1, &mut x, &mut stack).is_ok());
    }

    #[test]
    fn set_bif_hook_replaces_the_hook() {
        let mut ctx = MinimalContext::new();
        ctx.set_bif_hook(Box::new(|_m, _f, _a, _args| {
            Err(beam_common::BeamError::BadArg)
        }));
        assert!(ctx.call_bif("x", "y", 0, &[]).is_err());
    }

    /// A context that implements only the two required trait methods, so the
    /// defaulted `alloc_words`/`apply` bodies are exercised.
    struct OnlyRequired;

    impl ExecutionContext for OnlyRequired {
        fn call_bif(
            &mut self,
            _module: &str,
            _function: &str,
            _arity: u32,
            _args: &[Eterm],
        ) -> Result<Eterm> {
            Err(beam_common::BeamError::BadArg)
        }

        fn ensure_heap(
            &mut self,
            _heap_need: u32,
            _live: u32,
            _x_regs: &mut [Eterm],
            _stack: &mut [Eterm],
        ) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_alloc_words_is_a_typed_unsupported_error() {
        // A context with no exposed heap cannot serve raw words.
        assert!(OnlyRequired.alloc_words(8).is_err());
    }

    #[test]
    fn default_apply_forwards_to_call_bif() {
        // The default `apply` delegates to `call_bif`; here `call_bif` always
        // fails, and the failure must surface identically through `apply`.
        assert!(OnlyRequired.apply("erlang", "self", 0, &[]).is_err());
    }

    #[test]
    fn minimal_context_alloc_words_is_ok_and_non_null() {
        let mut ctx = MinimalContext::new();
        let ptr = ctx.alloc_words(4).expect("arena allocates");
        assert!(!ptr.is_null(), "a non-empty block has a non-null pointer");
    }

    #[test]
    fn minimal_context_zero_word_allocation_is_non_null() {
        // Documented: a zero-word request may return a dangling, but non-null,
        // well-aligned pointer.
        let mut ctx = MinimalContext::new();
        let ptr = ctx.alloc_words(0).expect("arena allocates an empty block");
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % core::mem::align_of::<Eterm>(), 0);
    }

    #[test]
    fn minimal_context_allocations_do_not_alias() {
        let mut ctx = MinimalContext::new();
        let a = ctx.alloc_words(4).expect("first alloc");
        let b = ctx.alloc_words(4).expect("second alloc");
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b, "distinct allocations own distinct storage");
    }

    #[test]
    fn minimal_context_apply_forwards_to_the_hook() {
        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, _args| {
            assert_eq!((m, f, a), ("lists", "map", 2));
            Ok(Eterm::nil())
        }));
        assert_eq!(ctx.apply("lists", "map", 2, &[]).unwrap(), Eterm::nil());
    }
}
