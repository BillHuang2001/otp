//! Cross-module call seam tests: a `call_ext*` the host resolves through
//! [`ExecutionContext::resolve_external_call`] is executed **in the calling
//! machine**, so a `receive` inside the cross-module callee suspends the *calling*
//! process ([`RunOutcome::Wait`]) instead of folding into the typed
//! [`error::blocked`](beam_engine::blocked) error at the call site.
//!
//! # What the seam changes
//!
//! Without the seam a `call_ext*` is a **synchronous** BIF dispatch: the engine
//! hands `module:function/arity` to [`ExecutionContext::call_bif`] and the host
//! runs the callee elsewhere (or emulates it). A callee that would block cannot
//! suspend its caller — the block has nowhere to go but the call site's error
//! path. With the seam the host answers `Some((callee_module, entry_label))` and
//! the engine enters the callee's code **in this machine**: the callee's `wait`
//! reports a `RunOutcome::Wait` for *this* process, its `return` unwinds back into
//! the caller's frame, and its raise reports its own reason.
//!
//! # Hand-built modules
//!
//! The seam passes plain `(module, function, arity)` **strings**, so no shared
//! [`AtomTable`](beam_loader::Module::atom_table) is needed: each module carries
//! its own table and the host matches on the callee's name. The synthetic modules
//! below are the smallest bytecode shapes that reproduce the compiled code's frame
//! discipline.
//!
//! The caller `m:run/0` allocates its own frame (so the continuation the callee
//! returns to lives there), `call_ext`s the callee, then `deallocate`/`return`s:
//!
//! ```text
//! m:run/0:
//!     allocate 0
//!     call_ext 0 c:<callee>/0      ; stores the continuation in m's frame
//!     deallocate 0
//!     return
//! ```
//!
//! The callee `c:recv/0` (function index `1`, after a trivial `noop/0` so the
//! suspension address is unambiguously the callee's) is a bare `receive`:
//!
//! ```text
//! c:recv/0:
//!     loop_rec wait 1 x0           ; candidate, or fall to the wait
//!     remove_message
//!     return
//!     wait 1                       ; resume at the loop_rec once a message arrives
//! ```
//!
//! A callee that raises (`c:raise/0`) enters a **1-Y-slot frame** and tail-calls
//! `erlang:error/1` with a **stale** `deallocate` — the shape that would surface a
//! `deallocate: frame size mismatch` artifact if the frame were popped before the
//! raise (C keeps the frame live across the BIF, `emu/bif_instrs.tab:262`).

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result};
use beam_engine::{blocked, ExecutionContext, Machine, MinimalContext, RunOutcome, Tier};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for one synthetic module with its own atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn import(&mut self, module_name: &str, function_name: &str, arity: u32) -> ImportRef {
        let module = self.atom(module_name);
        let function = self.atom(function_name);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    /// Append a function; its entry label is the first label in `code`.
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// The caller module: `run/0` allocates its own frame, `call_ext`s the named
/// callee export at arity 0, then `deallocate`/`return`s.
fn caller_module(callee_function: &str) -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let target = f.import("c", callee_function, 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::CallExt { target, arity: 0 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The callee module `c`: `noop/0` (function index `0`), `recv/0` (index `1`,
/// suspended on an empty mailbox), `raise/0` (index `2`, raises `erlang:error/1`
/// from a live frame with a stale `deallocate`) and `tailapply/0` (index `3`, a
/// tail `apply_last` whose return address is the caller's).
fn callee_module() -> Arc<Module> {
    let mut f = Fixture::new("c");

    let noop = f.add_function("noop", 0, vec![Instr::Label(Label(1)), Instr::Return]);
    f.add_export("noop", 0, noop);

    let recv = f.add_function(
        "recv",
        0,
        vec![
            Instr::Label(Label(10)),
            Instr::LoopRec {
                fail: Label(11),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(11)),
            Instr::Wait { target: Label(10) },
        ],
    );
    f.add_export("recv", 0, recv);

    let error = f.import("erlang", "error", 1);
    let raise = f.add_function(
        "raise",
        0,
        vec![
            Instr::Label(Label(20)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::CallExtLast {
                target: error,
                arity: 1,
                deallocate: 0,
            },
        ],
    );
    f.add_export("raise", 0, raise);

    // `tailapply/0` (function index `3`) — a callee whose body ends with a tail
    // `apply_last erlang:length/1`. Its `allocate` frame is popped by the
    // `apply_last`, so the continuation control then follows is the **caller's**
    // (written by the `call_ext`), i.e. an address in module context `0` (`m`).
    let erlang = f.atom("erlang");
    let length = f.atom("length");
    let tailapply = f.add_function(
        "tailapply",
        0,
        vec![
            Instr::Label(Label(30)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 3,
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Atom(erlang),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Atom(length),
                dst: Operand::X(2),
            },
            Instr::ApplyLast {
                arity: 1,
                deallocate: 0,
            },
        ],
    );
    f.add_export("tailapply", 0, tailapply);

    Arc::new(f.finish())
}

// ---------------------------------------------------------------------------
// A custom ExecutionContext implementing the cross-module seam
// ---------------------------------------------------------------------------

type BifFloor = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>;

/// A hand-written context that serves the cross-module seam plus a tiny mailbox.
///
/// * `callees` — the modules the seam resolves, keyed by name. A `call_ext*` to a
///   name in this list returns `Some((Arc<Module>, export_label))`; anything else
///   returns `Ok(None)` (the synchronous path).
/// * `seam` — a switch to turn the resolver off entirely, so the same host can
///   prove the **unchanged** behaviour a host without the seam still gets.
/// * `messages`/`scan` — a faithful mailbox: `receive_next` peeks at the scan
///   position, `receive_skip` advances it, `receive_remove` unlinks the current
///   message and `receive_reset` repositions at the first.
/// * `bif` — a pluggable BIF floor (the synchronous `call_ext*` target when the
///   seam declines).
struct SeamCtx {
    callees: Vec<(String, Arc<Module>)>,
    seam: bool,
    messages: Vec<Eterm>,
    scan: usize,
    bif: BifFloor,
}

impl SeamCtx {
    fn new(callees: Vec<(String, Arc<Module>)>, bif: BifFloor) -> SeamCtx {
        SeamCtx {
            callees,
            seam: true,
            messages: Vec::new(),
            scan: 0,
            bif,
        }
    }

    fn without_seam(mut self) -> SeamCtx {
        self.seam = false;
        self
    }

    fn with_messages(mut self, messages: impl IntoIterator<Item = Eterm>) -> SeamCtx {
        self.messages = messages.into_iter().collect();
        self
    }
}

impl ExecutionContext for SeamCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        (self.bif)(module, function, arity, args)
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

    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        if !self.seam {
            return Ok(None);
        }
        for (name, callee) in &self.callees {
            if name != module {
                continue;
            }
            let Some(function) = callee.atom_table.lookup(function) else {
                continue;
            };
            if let Some(label) = callee.export(function, arity) {
                return Ok(Some((Arc::clone(callee), label)));
            }
        }
        Ok(None)
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.messages.get(self.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        if self.scan < self.messages.len() {
            self.messages.remove(self.scan);
        }
        self.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan = 0;
        Ok(())
    }
}

/// A BIF floor that rejects everything (the synchronous `call_ext*` path is never
/// taken when the seam resolves the call).
fn rejecting() -> BifFloor {
    Box::new(|module, function, _arity, _args| {
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

/// A BIF floor that models `erlang:error/1` by raising with the argument term as
/// the **live reason** — the callee's own reason, carried out verbatim.
fn raising_error_bif() -> BifFloor {
    Box::new(|module, function, arity, args| {
        if module == "erlang" && function == "error" && arity == 1 && args.len() == 1 {
            return Err(BeamError::Raise(
                Raise::new(ExceptionClass::Error, ExitReason::Structured)
                    .with_live_reason(LiveReason::new(args[0])),
            ));
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

fn callee() -> Vec<(String, Arc<Module>)> {
    vec![("c".to_string(), callee_module())]
}

fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("a typed engine error names its reason")
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (a) a cross-module callee that blocks suspends the CALLER
// ---------------------------------------------------------------------------

#[test]
fn a_receive_inside_a_cross_module_callee_suspends_the_caller() {
    // `m:run/0` `call_ext`s `c:recv/0` on an empty mailbox. Because the host
    // resolved the call through the seam, the callee runs in this machine and its
    // `wait` suspends *this* process: a `RunOutcome::Wait`, NOT the folded
    // `error::blocked()` a synchronous host could only produce.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(callee(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");

    // The suspension address is the **callee's** `wait` (`c:recv/0` is function
    // index 1; `Label(10)` is its first instruction), proving the block happened
    // inside the cross-module callee and not at the caller's call site.
    assert_eq!(outcome, RunOutcome::Wait { func: 1, pc: 0 });
}

// ---------------------------------------------------------------------------
// (b) the same call completes when a message is already available
// ---------------------------------------------------------------------------

#[test]
fn a_cross_module_receive_returns_the_matched_message_to_the_caller() {
    // With a message already in the mailbox the callee's `loop_rec` matches it, the
    // callee `return`s, and control unwinds through the caller's frame (its
    // continuation, written by the `call_ext`) into the caller's `deallocate`/
    // `return` — the cross-module return path.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(callee(), rejecting()).with_messages([make_small(42)]);
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a completed call is not an error");

    assert_eq!(outcome, RunOutcome::Returned(make_small(42)));
    assert_eq!(machine.frame_depth(), 1, "the caller's frame was deallocated");
}

// ---------------------------------------------------------------------------
// (c) a raising cross-module callee reports its OWN reason
// ---------------------------------------------------------------------------

#[test]
fn a_cross_module_callee_raise_reports_the_callees_own_reason() {
    // `m:run/0` `call_ext`s `c:raise/0`, which allocates a 1-Y-slot frame and
    // tail-calls `erlang:error/1` with a STALE `deallocate 0`. C keeps the frame
    // live across the BIF, so the callee's own reason (`7`, the value it passed)
    // must surface — never a `deallocate: frame size mismatch` artifact and never a
    // substituted atom.
    let (caller, run) = caller_module("raise");
    let mut ctx = SeamCtx::new(callee(), raising_error_bif());
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect_err("the callee raises");

    match err {
        BeamError::Raise(raise) => {
            assert_eq!(raise.class, ExceptionClass::Error);
            assert_eq!(
                raise.reason.as_atom(),
                None,
                "a term reason is structured, not collapsed to an atom"
            );
            let reason = raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
                .copied();
            assert_eq!(
                reason,
                Some(make_small(7)),
                "the callee's own reason term, not a frame/deallocate artifact"
            );
        }
        other => panic!("expected the callee's raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (c2) a tail `apply_last` in the callee returns into the CALLER's module context
// ---------------------------------------------------------------------------

#[test]
fn a_tail_apply_inside_a_cross_module_callee_returns_to_the_callers_module() {
    // `m:run/0` `call_ext`s `c:tailapply/0`, whose body ends with a tail
    // `apply_last erlang:length/1`. The `apply_last` pops its own frame, leaving the
    // CALLER's frame whose continuation the `call_ext` wrote — an address in module
    // context `0` (`m`), i.e. a DIFFERENT module than the callee currently running
    // in. The `ApplyLast` arm must therefore report the context change
    // (`Step::Enter { context: 0, .. }`) so the driver re-binds `m` before executing
    // the return address. Dropping the context and reporting a bare `Step::Goto`
    // would make the driver follow `(func, pc)` in the **callee's** module `c` — its
    // function `0` (`noop/0`) has no instruction at that `pc`, so the run would fail
    // with a typed `run: pc out of range` instead of returning the applied value.
    let (caller, run) = caller_module("tailapply");
    let mut ctx = SeamCtx::new(
        callee(),
        Box::new(|module, function, arity, args| {
            if module == "erlang"
                && function == "length"
                && arity == 1
                && args.len() == 1
                && args[0] == make_small(1)
            {
                return Ok(make_small(41));
            }
            Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
        }),
    );
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the tail apply must return into the caller's module, not the callee's");

    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(41)),
        "the applied value, returned through the CALLER's `deallocate`/`return`"
    );
    assert_eq!(machine.frame_depth(), 1, "the caller's frame was deallocated");
}

// ---------------------------------------------------------------------------
// (d) a host WITHOUT the seam keeps today's behaviour
// ---------------------------------------------------------------------------

#[test]
fn without_the_seam_a_cross_module_call_is_synchronous() {
    // The `Ok(None)` default: the callee is dispatched through `call_bif` as
    // before, so the caller sees the synchronous result.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(
        callee(),
        Box::new(|module, function, arity, _args| {
            if module == "c" && function == "recv" && arity == 0 {
                return Ok(make_small(99));
            }
            Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
        }),
    )
    .without_seam();
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the synchronous call succeeds");

    assert_eq!(outcome, RunOutcome::Returned(make_small(99)));
}

#[test]
fn without_the_seam_a_blocking_callee_folds_into_the_typed_blocked_error() {
    // Without the seam a cross-module callee cannot suspend its caller: a host that
    // would block folds into the typed `blocked()` at the call site — the exact
    // behaviour this seam exists to lift.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(
        callee(),
        Box::new(|module, function, arity, _args| {
            if module == "c" && function == "recv" && arity == 0 {
                return Err(blocked());
            }
            Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
        }),
    )
    .without_seam();
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect_err("a host without the seam cannot suspend");

    assert_eq!(
        reason(err),
        "unsupported_opcode: run: process blocked (no message available)"
    );
}

// ---------------------------------------------------------------------------
// (e) a `Wait` inside the callee is resumable through the ENTRY module
// ---------------------------------------------------------------------------

#[test]
fn a_wait_inside_a_cross_module_callee_resumes_to_a_returned_value() {
    // The suspension must survive across the block: the machine keeps the callee
    // module as a module context, so `resume_outcome` — called with the run's
    // ENTRY module (context 0) — re-enters the callee's own `(func, pc)` once a
    // message is available, and the callee's `return` unwinds back into the caller.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(callee(), rejecting());
    let mut machine = Machine::new();

    let suspended = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");
    assert_eq!(suspended, RunOutcome::Wait { func: 1, pc: 0 });

    // A message arrives; resume. The entry module is the run's `m` module (context
    // 0); the `(func, pc)` are the callee's, recovered from the suspended context.
    ctx.messages.push(make_small(77));
    let resumed = machine
        .resume_outcome(&caller, 1, 0, &mut ctx)
        .expect("the resume completes");
    assert_eq!(resumed, RunOutcome::Returned(make_small(77)));
}

// ---------------------------------------------------------------------------
// the interpreter tier entry and the documented native-tier limitation
// ---------------------------------------------------------------------------

#[test]
fn the_interpreter_tier_entry_reports_the_same_cross_module_suspend() {
    // The tier-selecting outcome entry with `Tier::Interpreter` is the same
    // suspension a plain `run_export_outcome` reports.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(callee(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome_with_tier(&caller, run, 0, &[], &mut ctx, Tier::Interpreter)
        .expect("a suspend is not an error");

    assert_eq!(outcome, RunOutcome::Wait { func: 1, pc: 0 });
}

#[test]
fn a_native_tier_reports_a_typed_error_for_a_resolved_cross_module_call() {
    // A module-context switch is an interpreter-driver capability: a compiled
    // block's module, label map and literal area are all bound for ONE module. The
    // baseline tier must therefore report a typed error — never a silently wrong
    // block, and never a silent fallback to the synchronous path.
    let (caller, run) = caller_module("recv");
    let mut ctx = SeamCtx::new(callee(), rejecting());
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome_with_tier(&caller, run, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect_err("the native tier cannot switch module contexts");

    assert_eq!(
        reason(err),
        "unsupported_opcode: jit: module context switch outside the interpreter driver"
    );
}

// ---------------------------------------------------------------------------
// the seam's `Ok(None)` default is a no-op for any other external
// ---------------------------------------------------------------------------

#[test]
fn a_minimal_context_without_the_seam_keeps_the_synchronous_bif_path() {
    // The trait default (`Ok(None)`) is unchanged behaviour: a plain
    // `MinimalContext` still dispatches a `call_ext*` through its BIF hook.
    let (caller, run) = caller_module("recv");
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|module, function, arity, _args| {
        if module == "c" && function == "recv" && arity == 0 {
            return Ok(make_small(5));
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    }));
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the synchronous call succeeds");

    assert_eq!(outcome, RunOutcome::Returned(make_small(5)));
}
