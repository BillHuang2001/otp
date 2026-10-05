//! JIT tier seam: the execution tiers, the backend trait and the tier roster.
//!
//! The engine executes the frozen typed IR in one of three tiers, identified by
//! [`Tier`]:
//!
//! * [`Tier::Interpreter`] — the direct-threaded loop (`crate::interpreter`).
//!   Always compiled in, always first in [`available_tiers`]; the vertical slice
//!   runs on it. [`Machine::run_export`](crate::Machine::run_export) is and stays
//!   the default interpreter entry point.
//! * [`Tier::BaselineJit`] — the native baseline tier (a Rust port of BeamAsm with
//!   block register allocation). It is **built**: [`baseline::BaselineJit`] lowers
//!   the typed IR's basic blocks to x86-64 (via the encoder and executable code
//!   buffer in the [`emitter`] submodule) and runs a function through a driver loop
//!   that falls back to the interpreter's per-instruction step for anything it does
//!   not lower. See [`baseline`] for the block ABI, the register-allocation scheme
//!   and the inline/helper split.
//! * [`Tier::Cranelift`] — the optional hot tier (Cranelift codegen), gated behind
//!   the **`jit-cranelift`** Cargo feature (default **OFF**). Deliberately off the
//!   critical path: with the feature disabled the crate carries no Cranelift
//!   dependency at all and the build never depends on Cranelift codegen. JIT
//!   compilation is *measurement-gated* (Part II §F) with a bounded-latency,
//!   cheap-deopt design, and the backend is not built yet either — selecting the
//!   tier without a compiled-in backend is a typed error, never a panic.
//!
//! [`available_tiers`] reports the tier **names** compiled into this build, and
//! [`Tier::from_name`] maps a name back to a [`Tier`]; the two are kept consistent
//! by construction (a unit test asserts the roster is a prefix of [`Tier::ALL`]).
//!
//! # The `jit-beamasm` Cargo feature
//!
//! The historical `jit-beamasm` feature is retained as a **documented alias for
//! the baseline tier**, not as a separate tier: the baseline tier is part of every
//! build's roster (`"baseline-jit"` is always present after `"interpreter"`), so
//! enabling the feature changes nothing.
//!
//! # Backends
//!
//! A native tier implements [`JitBackend`]: it names itself ([`JitBackend::name`]),
//! reports whether it is usable in the current build
//! ([`JitBackend::available`]), compiles a whole [`Module`] into its own
//! [`JitBackend::Compiled`] artifact, and runs an exported function on a
//! [`Machine`]. The trait is the seam the runtime selects a tier through; the
//! interpreter is *not* a [`JitBackend`] (it has nothing to compile — it is
//! reached through [`Tier::Interpreter`] and
//! [`Machine::run_export`](crate::Machine::run_export)). [`baseline::BaselineJit`]
//! is the one backend built today.

use beam_common::Result;
use beam_loader::{Atom, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::Machine;

pub mod baseline;
pub mod emitter;

/// An execution tier of the typed IR.
///
/// The variants are ordered as the roster is ([`Tier::ALL`]): the interpreter
/// first, the optional Cranelift hot tier last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The direct-threaded interpreter (`crate::interpreter`).
    ///
    /// Always available; the reference semantics every other tier must match.
    Interpreter,
    /// The native baseline JIT (a Rust port of BeamAsm with block register
    /// allocation).
    ///
    /// Named `"baseline-jit"`. The backend is [`baseline::BaselineJit`]: it compiles
    /// the typed IR's basic blocks to x86-64 and runs the export through a driver
    /// loop with a per-instruction interpreter deopt.
    BaselineJit,
    /// The optional Cranelift hot tier.
    ///
    /// Named `"jit-cranelift"` and compiled in only when the **`jit-cranelift`**
    /// Cargo feature is enabled (default OFF). It is the *unavailable* tier until a
    /// backend is wired in: selecting it returns a typed
    /// [`unsupported`](crate::error::unsupported) error rather than falling back
    /// silently.
    Cranelift,
}

impl Tier {
    /// Every tier the engine knows about, in roster order (interpreter first).
    pub const ALL: [Tier; 3] = [Tier::Interpreter, Tier::BaselineJit, Tier::Cranelift];

    /// The tier's stable name, as reported by [`available_tiers`] and parsed by
    /// [`Tier::from_name`].
    pub fn name(&self) -> &'static str {
        match self {
            Tier::Interpreter => "interpreter",
            Tier::BaselineJit => "baseline-jit",
            Tier::Cranelift => "jit-cranelift",
        }
    }

    /// Parse a tier name back into a [`Tier`], or `None` if the name is unknown.
    ///
    /// The inverse of [`Tier::name`]; an unknown name is `None`, never a default
    /// tier — a typo in a configuration string must not silently pick a tier.
    pub fn from_name(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|tier| tier.name() == name)
    }

    /// Whether this tier is compiled into the current build.
    ///
    /// `true` exactly for the tiers [`available_tiers`] lists.
    pub fn is_available(&self) -> bool {
        available_tiers().contains(&self.name())
    }
}

/// A native execution tier for the typed IR.
///
/// A backend owns whatever state compiling and running the IR needs (a code
/// buffer, per-module caches, …), hence the `&mut self` receivers: [`compile`] may
/// be called again for another module, and [`run_export`] may compile lazily on
/// first use.
///
/// The engine never picks a backend implicitly: [`available_tiers`] reports what is
/// compiled in, [`Tier::is_available`] answers for one tier, and
/// [`Machine::run_with_tier`](crate::Machine::run_with_tier) is the entry that
/// selects a tier. A backend that cannot serve a request returns a typed
/// [`beam_common::BeamError`] — the engine never panics, and never falls back to a
/// *different* tier behind the caller's back.
///
/// [`compile`]: JitBackend::compile
/// [`run_export`]: JitBackend::run_export
pub trait JitBackend {
    /// The artifact [`compile`](JitBackend::compile) produces for one [`Module`].
    ///
    /// Left to the backend: a code buffer plus entry table, a lowered module, or a
    /// cache handle. The engine treats it as opaque.
    type Compiled;

    /// The tier's stable name (e.g. `"baseline-jit"`), as returned by
    /// [`available_tiers`].
    fn name(&self) -> &'static str;

    /// Whether this tier is usable in the current build.
    ///
    /// Mirrors [`Tier::is_available`]; a backend may also report `false` at runtime
    /// (e.g. a missing executable-memory facility), which must be a
    /// [`beam_common::BeamError`] at the call site rather than a panic.
    fn available(&self) -> bool;

    /// Compile `module` into this backend's [`Compiled`](JitBackend::Compiled)
    /// artifact.
    ///
    /// Compile latency is a first-class concern (Part II §C/§F): a backend must
    /// either stay bounded or decline with a typed error, never stall the caller.
    fn compile(&mut self, module: &Module) -> Result<Self::Compiled>;

    /// Run the exported `function/arity` on `machine`; yields `x(0)`.
    ///
    /// The backend owns the entry into its compiled code but borrows the engine's
    /// process state: `machine` supplies the register file / stack / frames and
    /// `ctx` the [`ExecutionContext`] seam (BIF dispatch, heap, fun application).
    /// Failure is return-based: a missing export, an unsupported instruction or a
    /// raising BIF is an `Err`, never a panic.
    fn run_export(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm>;
}

/// The execution tiers compiled into this build.
///
/// Exactly `["interpreter", "baseline-jit"]`, plus `"jit-cranelift"` when the
/// **`jit-cranelift`** feature is enabled. The interpreter is always present and
/// always first; the roster is a prefix of [`Tier::ALL`] mapped to
/// [`Tier::name`].
pub fn available_tiers() -> &'static [&'static str] {
    TIERS
}

/// The roster of tiers compiled into this build (interpreter first).
#[cfg(feature = "jit-cranelift")]
const TIERS: &[&str] = &["interpreter", "baseline-jit", "jit-cranelift"];

/// The roster of tiers compiled into this build (interpreter first).
#[cfg(not(feature = "jit-cranelift"))]
const TIERS: &[&str] = &["interpreter", "baseline-jit"];

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_loader::load_file;
    use beam_term::{make_small, signed_value, Atom};

    /// The `beam_loader` oracle fixture (reused, never copied).
    const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    /// Resolve a name interned in the module's atom table.
    fn atom(module: &Module, name: &str) -> Atom {
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
    }

    /// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
    fn arith_hook(
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> core::result::Result<Eterm, BeamError> {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        if !beam_term::is_small(args[0]) || !beam_term::is_small(args[1]) {
            return Err(BeamError::BadArith);
        }
        let (a, b) = (signed_value(args[0]), signed_value(args[1]));
        match function {
            "+" => Ok(make_small(a + b)),
            "-" => Ok(make_small(a - b)),
            _ => Err(BeamError::BadArg),
        }
    }

    fn slice_module() -> Module {
        load_file(SLICE_BEAM).expect("load slice.beam")
    }

    /// Run `slice:add/2` on `tier`.
    fn run_add(tier: Tier) -> Result<Eterm> {
        let module = slice_module();
        let mut machine = crate::Machine::new();
        let mut ctx = crate::MinimalContext::with_bif_hook(Box::new(arith_hook));
        machine.run_with_tier(
            &module,
            atom(&module, "add"),
            2,
            &[make_small(2), make_small(3)],
            &mut ctx,
            tier,
        )
    }

    #[test]
    fn interpreter_is_always_first_and_the_roster_is_a_tier_prefix() {
        let tiers = available_tiers();
        assert_eq!(tiers.first().copied(), Some("interpreter"));
        assert!(!tiers.is_empty());

        // Every roster name parses back to a tier, in `Tier::ALL` order.
        let names: Vec<&str> = Tier::ALL.iter().map(|tier| tier.name()).collect();
        assert_eq!(tiers, &names[..tiers.len()]);
    }

    #[test]
    fn baseline_jit_tier_is_part_of_every_build() {
        assert!(available_tiers().contains(&"baseline-jit"));
        assert_eq!(Tier::from_name("baseline-jit"), Some(Tier::BaselineJit));
        assert!(Tier::BaselineJit.is_available());
    }

    #[test]
    fn cranelift_tier_tracks_the_feature() {
        let has = available_tiers().contains(&"jit-cranelift");
        assert_eq!(has, cfg!(feature = "jit-cranelift"));
        assert_eq!(Tier::Cranelift.is_available(), has);
    }

    #[test]
    fn tier_names_round_trip() {
        for tier in Tier::ALL {
            assert_eq!(Tier::from_name(tier.name()), Some(tier));
        }
        assert_eq!(Tier::Interpreter.name(), "interpreter");
        assert_eq!(Tier::BaselineJit.name(), "baseline-jit");
        assert_eq!(Tier::Cranelift.name(), "jit-cranelift");
        // An unknown name is `None`, never a default tier.
        assert_eq!(Tier::from_name("jit-beamasm"), None);
        assert_eq!(Tier::from_name(""), None);
        assert_eq!(Tier::from_name("Interpreter"), None);
    }

    #[test]
    fn run_with_tier_interpreter_matches_run_export() {
        let module = slice_module();
        let add = atom(&module, "add");
        let args = [make_small(2), make_small(3)];

        let mut direct = crate::Machine::new();
        let mut ctx = crate::MinimalContext::with_bif_hook(Box::new(arith_hook));
        let expected = direct
            .run_export(&module, add, 2, &args, &mut ctx)
            .expect("add(2, 3) via run_export");

        let mut tiered = crate::Machine::new();
        let mut ctx = crate::MinimalContext::with_bif_hook(Box::new(arith_hook));
        let got = tiered
            .run_with_tier(&module, add, 2, &args, &mut ctx, Tier::Interpreter)
            .expect("add(2, 3) via run_with_tier");

        assert_eq!(got, expected);
        assert_eq!(got, make_small(5));
    }

    #[test]
    fn baseline_tier_runs_the_module_on_the_native_backend() {
        // The baseline tier now compiles and runs native code; its observable result
        // must be the interpreter's.
        assert_eq!(run_add(Tier::BaselineJit).expect("add(2, 3)"), make_small(5));
    }

    #[test]
    fn cranelift_tier_is_a_typed_error_not_a_silent_fallback() {
        let err = run_add(Tier::Cranelift).expect_err("no Cranelift backend is built");
        assert!(matches!(err, BeamError::Raise(_)));
    }
}
