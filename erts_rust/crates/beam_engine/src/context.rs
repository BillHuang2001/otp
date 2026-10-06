//! The runtime seam: everything the engine needs from its host.
//!
//! [`ExecutionContext`] isolates BIF dispatch (`call_bif`), heap management
//! (`ensure_heap`) and the **mailbox** (`send_message`/`receive_*`) behind a trait,
//! so the Stage-1 engine depends only on `beam_loader` and never on the Stage-5
//! runtime core (`../CONTEXT.md`, "The seam types"). `beam_rt` supplies the
//! concrete implementation; [`MinimalContext`] is the test/embedding
//! implementation (unbounded heap plus a pluggable BIF hook and mailbox).
//!
//! The same trait also exposes the hooks the term-building layer
//! ([`crate::terms`]) and the call paths need: [`alloc_words`] hands out a stable
//! pointer to raw heap words, [`apply`] is the general external-function dispatch
//! entry point, and [`apply_fun`] applies a fun/closure value (the `call_fun`/
//! `apply` path). All are added with default bodies, so an existing implementor
//! keeps compiling unchanged.
//!
//! ## The mailbox / message-receive seam
//!
//! The `receive` opcode family ([`crate::families::receive`]) never looks at a
//! message's *contents*: the compiled code's match tests decide whether the
//! current candidate fits, and the family only ever **positions** the mailbox scan
//! (`loop_rec`/`loop_rec_end`), **removes** the matched candidate
//! (`remove_message`) or resets the position (`timeout`), plus the four
//! `recv_marker_*` bookkeeping opcodes. That host interface is [`Mailbox`], and
//! [`ExecutionContext`] carries one defaulted method per entry (each returning a
//! typed [`crate::error::unsupported`] out of the box) so any implementor — the
//! production `beam_rt` context, a test harness or [`MinimalContext`] — compiles
//! unchanged and opts in only to what it can serve.
//!
//! A **receive marker** term is fully **opaque** to the engine: it is produced by
//! [`receive_marker_reserve`](ExecutionContext::receive_marker_reserve), handed
//! back verbatim by `recv_marker_bind`, and only a separate *reference* term is
//! ever passed to `recv_marker_use`/`recv_marker_clear`. The engine stores such a
//! marker in a register and never inspects, compares or hashes it.
//!
//! Installing a mailbox does **not** change the semantics of the non-blocking
//! entries: they cannot suspend, so a `receive` that blocks still surfaces as the
//! typed [`crate::error::blocked`] — the mailbox only decides *whether* the scan
//! blocks, never what a blocking run reports.
//!
//! [`alloc_words`]: ExecutionContext::alloc_words
//! [`apply`]: ExecutionContext::apply
//! [`apply_fun`]: ExecutionContext::apply_fun

use beam_common::Result;
use beam_term::Eterm;

/// The mailbox/message-receive seam: the receive opcode family's host interface.
///
/// One method per receive opcode group; `beam_engine` drives them at instruction
/// boundaries and keeps no mailbox state of its own. The scan position is
/// entirely the host's: the engine never learns *which* message is current, only
/// the candidate [`receive_next`](Mailbox::receive_next) returns.
///
/// [`ExecutionContext`] exposes one defaulted method per entry below, so a host
/// implements the mailbox either by overriding those methods directly or — like
/// [`MinimalContext`] — by owning a `Box<dyn Mailbox>` and forwarding to it.
pub trait Mailbox {
    /// `send`: deliver `message` to the pid/port term `to`.
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()>;

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)`
    /// when the scan/mailbox is exhausted.
    fn receive_next(&mut self) -> Result<Option<Eterm>>;

    /// `loop_rec_end`: advance the scan position past the current candidate.
    fn receive_skip(&mut self) -> Result<()>;

    /// `remove_message`: unlink the currently-matched candidate and reposition the
    /// scan at the first message.
    fn receive_remove(&mut self) -> Result<()>;

    /// `timeout`: reset the scan position to the first message.
    fn receive_reset(&mut self) -> Result<()>;

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    fn receive_marker_reserve(&mut self) -> Result<Eterm>;

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference
    /// `reference`.
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()>;

    /// `recv_marker_use`: set the scan position to the position `reference`'s
    /// marker recorded.
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()>;

    /// `recv_marker_clear`: release `reference`'s marker.
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()>;
}

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
    ///
    /// # Required x-register layout (frozen)
    ///
    /// The engine passes the **call arguments** in `args` (already read from
    /// `x(0..arity)`); the context must enter the closure's target with
    /// `x(0..arity)` = those call arguments, and then `x(arity..)` = the
    /// closure's **free variables** — the C `call_fun` order
    /// (`reg[i] = args[i]` for `i < arity`, then `reg[i + arity] = funp->env[i]`,
    /// `../../erts/emulator/beam/beam_common.c:1631-1633`).
    ///
    /// Entering with the captures **before** the arguments (`free ++ args`) is a
    /// **silent** wrong-result / infinite-loop bug: the target reads `x(k)` for
    /// `k < arity` and gets a capture instead of an argument. A closure with
    /// **zero** captures cannot detect this (both layouts coincide), so the bug is
    /// invisible to zero-capture funs and only surfaces with a genuinely capturing
    /// closure.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let _ = (fun, args);
        Err(crate::error::unsupported("apply_fun"))
    }

    /// `send`: deliver `message` to the pid/port term `to` (the `send` opcode).
    ///
    /// The default body is a typed [`crate::error::unsupported`] — a context with
    /// no mailbox cannot send. On `Ok` the `send` opcode stores the **message
    /// itself** in `x(0)` (C's `erl_send` returns the message, and
    /// `Pid ! Msg` evaluates to `Msg`); on `Err` the `send` opcode has no fail
    /// label, so the family raises it through the catch-aware path.
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        let _ = (to, message);
        Err(crate::error::unsupported("send_message"))
    }

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)`
    /// when the scan/mailbox is exhausted.
    ///
    /// The default body is a typed [`crate::error::unsupported`]. The engine does
    /// **not** match the candidate against the receive's patterns — the compiled
    /// match tests that follow `loop_rec` do that; the family only moves the
    /// candidate into the instruction's destination register.
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Err(crate::error::unsupported("receive_next"))
    }

    /// `loop_rec_end`: advance the scan position past the current candidate.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_skip(&mut self) -> Result<()> {
        Err(crate::error::unsupported("receive_skip"))
    }

    /// `remove_message`: unlink the currently-matched candidate and reposition the
    /// scan at the first message.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_remove(&mut self) -> Result<()> {
        Err(crate::error::unsupported("receive_remove"))
    }

    /// `timeout`: reset the scan position to the first message.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_reset(&mut self) -> Result<()> {
        Err(crate::error::unsupported("receive_reset"))
    }

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    ///
    /// The returned term is **opaque** to the engine (it is stored in a register
    /// and only ever handed back through
    /// [`receive_marker_bind`](ExecutionContext::receive_marker_bind)); the engine
    /// never inspects, compares or hashes it. The default body is a typed
    /// [`crate::error::unsupported`].
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Err(crate::error::unsupported("receive_marker_reserve"))
    }

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference
    /// `reference`.
    ///
    /// The engine passes both terms through verbatim and never inspects either.
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        let _ = (marker, reference);
        Err(crate::error::unsupported("receive_marker_bind"))
    }

    /// `recv_marker_use`: set the scan position to the position `reference`'s
    /// marker recorded.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        let _ = reference;
        Err(crate::error::unsupported("receive_marker_use"))
    }

    /// `recv_marker_clear`: release `reference`'s marker.
    ///
    /// The default body is a typed [`crate::error::unsupported`].
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        let _ = reference;
        Err(crate::error::unsupported("receive_marker_clear"))
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
///
/// A **mailbox** ([`with_mailbox`](MinimalContext::with_mailbox) /
/// [`set_mailbox`](MinimalContext::set_mailbox)) may be installed to serve the
/// nine receive-seam methods; without one each stays the trait's typed
/// `unsupported`.
pub struct MinimalContext {
    hook: BifHook,
    /// An optional fun-application hook (the [`ExecutionContext::apply_fun`] body).
    apply_hook: Option<ApplyHook>,
    /// An optional mailbox (the body of the nine receive-seam methods).
    mailbox: Option<Box<dyn Mailbox>>,
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
            mailbox: None,
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

    /// A context with the given mailbox and the default (rejecting) BIF hook.
    ///
    /// Combine with a BIF hook via [`set_bif_hook`](MinimalContext::set_bif_hook).
    pub fn with_mailbox(mailbox: Box<dyn Mailbox>) -> MinimalContext {
        let mut ctx = MinimalContext::new();
        ctx.mailbox = Some(mailbox);
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

    /// Install (or replace) the mailbox.
    pub fn set_mailbox(&mut self, mailbox: Box<dyn Mailbox>) {
        self.mailbox = Some(mailbox);
    }

    /// Forward to the installed mailbox, or return the receive seam's typed
    /// `unsupported` naming `method`.
    ///
    /// The single definition every receive-seam override below shares, so the
    /// "no mailbox installed" contract stays byte-identical to the trait defaults.
    fn mailbox_call<T>(
        &mut self,
        method: &str,
        call: impl FnOnce(&mut dyn Mailbox) -> Result<T>,
    ) -> Result<T> {
        match self.mailbox.as_mut() {
            Some(mailbox) => call(mailbox.as_mut()),
            None => Err(crate::error::unsupported(method)),
        }
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

    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        self.mailbox_call("send_message", |mailbox| mailbox.send_message(to, message))
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        self.mailbox_call("receive_next", |mailbox| mailbox.receive_next())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.mailbox_call("receive_skip", |mailbox| mailbox.receive_skip())
    }

    fn receive_remove(&mut self) -> Result<()> {
        self.mailbox_call("receive_remove", |mailbox| mailbox.receive_remove())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.mailbox_call("receive_reset", |mailbox| mailbox.receive_reset())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        self.mailbox_call("receive_marker_reserve", |mailbox| {
            mailbox.receive_marker_reserve()
        })
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        self.mailbox_call("receive_marker_bind", |mailbox| {
            mailbox.receive_marker_bind(marker, reference)
        })
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        self.mailbox_call("receive_marker_use", |mailbox| {
            mailbox.receive_marker_use(reference)
        })
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        self.mailbox_call("receive_marker_clear", |mailbox| {
            mailbox.receive_marker_clear(reference)
        })
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

    // ---- the mailbox / receive seam ---------------------------------------

    /// The shared call log of a [`LoggingMailbox`].
    type Log = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    /// A mailbox that records every seam call as a string (so forwarding through
    /// the context's boxed trait object is observable) and answers with fixed
    /// results.
    struct LoggingMailbox {
        log: Log,
        next: Option<Eterm>,
    }

    impl LoggingMailbox {
        /// A mailbox logging to a fresh log, with no candidate available.
        fn new() -> (Log, LoggingMailbox) {
            let log: Log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            (std::rc::Rc::clone(&log), LoggingMailbox { log, next: None })
        }

        /// Record one call (`format!`ed, so argument passthrough is visible too).
        fn record(&mut self, call: String) {
            self.log.borrow_mut().push(call);
        }
    }

    impl Mailbox for LoggingMailbox {
        fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
            self.record(format!("send_message {to:?} {message:?}"));
            Ok(())
        }

        fn receive_next(&mut self) -> Result<Option<Eterm>> {
            self.record("receive_next".to_string());
            Ok(self.next)
        }

        fn receive_skip(&mut self) -> Result<()> {
            self.record("receive_skip".to_string());
            Ok(())
        }

        fn receive_remove(&mut self) -> Result<()> {
            self.record("receive_remove".to_string());
            Ok(())
        }

        fn receive_reset(&mut self) -> Result<()> {
            self.record("receive_reset".to_string());
            Ok(())
        }

        fn receive_marker_reserve(&mut self) -> Result<Eterm> {
            self.record("receive_marker_reserve".to_string());
            Ok(Eterm::from_raw(0xABCD))
        }

        fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
            self.record(format!("receive_marker_bind {marker:?} {reference:?}"));
            Ok(())
        }

        fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
            self.record(format!("receive_marker_use {reference:?}"));
            Ok(())
        }

        fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
            self.record(format!("receive_marker_clear {reference:?}"));
            Ok(())
        }
    }

    #[test]
    fn the_receive_seam_defaults_are_typed_unsupported_errors() {
        // A context that overrides nothing (the `OnlyRequired` stub) gets all nine
        // receive-seam methods as the trait's documented typed `unsupported`, so
        // every existing implementor keeps compiling and behaves predictably.
        let mut ctx = OnlyRequired;
        let e = Eterm::from_raw(7);
        assert!(matches!(
            ctx.send_message(e, e).unwrap_err(),
            beam_common::BeamError::Raise(_)
        ));
        assert!(ctx.receive_next().is_err());
        assert!(ctx.receive_skip().is_err());
        assert!(ctx.receive_remove().is_err());
        assert!(ctx.receive_reset().is_err());
        assert!(ctx.receive_marker_reserve().is_err());
        assert!(ctx.receive_marker_bind(e, e).is_err());
        assert!(ctx.receive_marker_use(e).is_err());
        assert!(ctx.receive_marker_clear(e).is_err());
    }

    #[test]
    fn minimal_context_without_a_mailbox_keeps_the_typed_unsupported_defaults() {
        // The nine methods name themselves in the reason, exactly as the defaults do.
        let mut ctx = MinimalContext::new();
        let e = Eterm::from_raw(7);
        for (reason, result) in [
            ("send_message", ctx.send_message(e, e).err()),
            ("receive_next", ctx.receive_next().err()),
            ("receive_skip", ctx.receive_skip().err()),
            ("receive_remove", ctx.receive_remove().err()),
            ("receive_reset", ctx.receive_reset().err()),
            ("receive_marker_reserve", ctx.receive_marker_reserve().err()),
            ("receive_marker_bind", ctx.receive_marker_bind(e, e).err()),
            ("receive_marker_use", ctx.receive_marker_use(e).err()),
            ("receive_marker_clear", ctx.receive_marker_clear(e).err()),
        ] {
            match result.expect("no mailbox ⇒ typed error") {
                beam_common::BeamError::Raise(raise) => assert_eq!(
                    raise.reason.as_atom(),
                    Some(format!("unsupported_opcode: {reason}").as_str()),
                    "the error names the seam method"
                ),
                other => panic!("expected a typed raise for {reason}, got {other:?}"),
            }
        }
    }

    #[test]
    fn minimal_context_forwards_every_receive_seam_call_to_the_installed_mailbox() {
        let (log, mailbox) = LoggingMailbox::new();
        let mut ctx = MinimalContext::with_mailbox(Box::new(mailbox));
        let e = Eterm::from_raw(9);

        ctx.send_message(e, e).unwrap();
        assert_eq!(ctx.receive_next().unwrap(), None);
        ctx.receive_skip().unwrap();
        ctx.receive_remove().unwrap();
        ctx.receive_reset().unwrap();
        assert_eq!(
            ctx.receive_marker_reserve().unwrap(),
            Eterm::from_raw(0xABCD)
        );
        ctx.receive_marker_bind(Eterm::from_raw(1), Eterm::from_raw(2))
            .unwrap();
        ctx.receive_marker_use(Eterm::from_raw(3)).unwrap();
        ctx.receive_marker_clear(Eterm::from_raw(4)).unwrap();

        assert_eq!(
            *log.borrow(),
            vec![
                format!("send_message {e:?} {e:?}"),
                "receive_next".to_string(),
                "receive_skip".to_string(),
                "receive_remove".to_string(),
                "receive_reset".to_string(),
                "receive_marker_reserve".to_string(),
                format!(
                    "receive_marker_bind {:?} {:?}",
                    Eterm::from_raw(1),
                    Eterm::from_raw(2)
                ),
                format!("receive_marker_use {:?}", Eterm::from_raw(3)),
                format!("receive_marker_clear {:?}", Eterm::from_raw(4)),
            ]
        );
    }

    #[test]
    fn set_mailbox_replaces_the_previous_mailbox() {
        // The first mailbox yields no candidate; after `set_mailbox` the second one
        // yields a term — proof the replacement took effect.
        let (_, first) = LoggingMailbox::new();
        let (_, mut second) = LoggingMailbox::new();
        second.next = Some(Eterm::from_raw(0x42));
        let mut ctx = MinimalContext::with_mailbox(Box::new(first));
        assert_eq!(ctx.receive_next().unwrap(), None);
        ctx.set_mailbox(Box::new(second));
        assert_eq!(ctx.receive_next().unwrap(), Some(Eterm::from_raw(0x42)));
    }
}
