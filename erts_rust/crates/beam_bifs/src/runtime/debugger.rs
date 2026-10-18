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

//! The whole 13-key `erl_debugger` family — the interpreter-level debugger tools.
//!
//! Ground truth: `../../erts/emulator/beam/erl_debugger.c`. Every entry point there
//! starts with `BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT()`, which is
//!
//! ```text
//! #define BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT() \
//!     if (!erts_debugger_flags) BIF_ERROR(BIF_P, EXC_UNDEF)
//! ```
//!
//! and `erts_debugger_flags` is **disabled in a default build** (the debugger is a
//! JIT/disassembly debugging facility compiled in only for a `DEBUG` emulator). So a
//! stock OTP answers the **bare atom `undef`** for twelve of the thirteen keys, for
//! *every* argument — the support guard precedes any argument inspection, so a
//! well-formed and a garbage argument are indistinguishable.
//!
//! The one exception is `supported/0`, which is the query the guard itself is about:
//! it returns `false` (`erl_debugger_supported_0`: `BIF_RET(am_false)` when the flags
//! are off).
//!
//! Both behaviours are host-independent, so this family is implemented **for real**
//! here rather than forwarding to the [`RuntimeHooks`](crate::context::RuntimeHooks)
//! seam: no host can change the answer without recompiling the emulator with a
//! debugger facility this runtime does not have.
//!
//! Oracle probe (OTP 29.1.1 / erts-17.1, `erl` on `PATH`; each expression wrapped in
//! `try EXPR catch C:R -> {C,R} end`):
//!
//! ```text
//! erl_debugger:supported()                                => false
//! erl_debugger:instrumentations()                         => {error,undef}
//! erl_debugger:toggle_instrumentations([])                => {error,undef}
//! erl_debugger:toggle_instrumentations(1)                 => {error,undef}
//! erl_debugger:register(self())                           => {error,undef}
//! erl_debugger:register(foo)                              => {error,undef}
//! erl_debugger:unregister(a,b)                            => {error,undef}
//! erl_debugger:unregister([],[])                          => {error,undef}
//! erl_debugger:whereis()                                  => {error,undef}
//! erl_debugger:breakpoint({lists,map,2},[])               => {error,undef}
//! erl_debugger:breakpoint(a,b,c)                          => {error,undef}
//! erl_debugger:breakpoints([])                            => {error,undef}
//! erl_debugger:breakpoints(x)                             => {error,undef}
//! erl_debugger:breakpoints([],a,b)                        => {error,undef}
//! erl_debugger:stack_frames(self(),0)                     => {error,undef}
//! erl_debugger:stack_frames(a,b)                          => {error,undef}
//! erl_debugger:peek_stack_frame_slot(1,2,3,4)             => {error,undef}
//! erl_debugger:peek_xreg(a,b,c)                           => {error,undef}
//! erl_debugger:xregs_count(self())                        => {error,undef}
//! erl_debugger:xregs_count(nonsense)                      => {error,undef}
//! ```
//!
//! `undef` is `crate::error::undefined_bif` — an `error`-class raise whose reason is
//! the bare atom `undef`, which is exactly what `EXC_UNDEF` produces; it is *not* the
//! typed `unsupported_bif` capability gap (the oracle really has no such function
//! available) and never `badarg` (no argument was inspected).

use beam_term::Eterm;

use crate::context::BifContext;
use crate::error::undefined_bif;
use crate::registry::BifRegistry;

/// The `(module, function, arity)` keys this module implements **for real** — the
/// whole 13-key `bif.tab` `erl_debugger` surface. Sorted by `(module, function,
/// arity)`.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erl_debugger", "breakpoint", 3),
    ("erl_debugger", "breakpoints", 1),
    ("erl_debugger", "breakpoints", 3),
    ("erl_debugger", "instrumentations", 0),
    ("erl_debugger", "peek_stack_frame_slot", 4),
    ("erl_debugger", "peek_xreg", 3),
    ("erl_debugger", "register", 1),
    ("erl_debugger", "stack_frames", 2),
    ("erl_debugger", "supported", 0),
    ("erl_debugger", "toggle_instrumentations", 1),
    ("erl_debugger", "unregister", 2),
    ("erl_debugger", "whereis", 0),
    ("erl_debugger", "xregs_count", 1),
];

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erl_debugger", "breakpoint", 3, no_debugger_support);
    reg.register("erl_debugger", "breakpoints", 1, no_debugger_support);
    reg.register("erl_debugger", "breakpoints", 3, no_debugger_support);
    reg.register(
        "erl_debugger",
        "instrumentations",
        0,
        no_debugger_support,
    );
    reg.register(
        "erl_debugger",
        "peek_stack_frame_slot",
        4,
        no_debugger_support,
    );
    reg.register("erl_debugger", "peek_xreg", 3, no_debugger_support);
    reg.register("erl_debugger", "register", 1, no_debugger_support);
    reg.register("erl_debugger", "stack_frames", 2, no_debugger_support);
    reg.register("erl_debugger", "supported", 0, supported_0);
    reg.register(
        "erl_debugger",
        "toggle_instrumentations",
        1,
        no_debugger_support,
    );
    reg.register("erl_debugger", "unregister", 2, no_debugger_support);
    reg.register("erl_debugger", "whereis", 0, no_debugger_support);
    reg.register("erl_debugger", "xregs_count", 1, no_debugger_support);
    // `REAL_BIFS` is the declared roster; keep it honest against what was registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

/// `erl_debugger:supported/0` — is the emulator built with the debugger facility?
///
/// `erl_debugger.c`'s `erl_debugger_supported_0` answers `am_false` when
/// `erts_debugger_flags` is off, which is every default build. Oracle-verified: the
/// atom `false` (not `ok`, not `[]`).
///
/// # Errors
///
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the atom
/// `false` is neither interned nor internable — the runtime's table always holds it.
fn supported_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> beam_common::Result<Eterm> {
    ctx.boolean(false)
}

/// The support guard every other `erl_debugger` key starts with:
/// `BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT()` raises the bare atom `undef` **before** any
/// argument is inspected, so the argument is deliberately ignored here too.
///
/// # Errors
///
/// Always `error:undef` (`crate::error::undefined_bif`).
fn no_debugger_support(
    _ctx: &mut BifContext<'_>,
    _args: &[Eterm],
) -> beam_common::Result<Eterm> {
    Err(undefined_bif())
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::{BeamError, ExitReason};
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    /// Runs one `erl_debugger` call over a fresh context and returns the outcome.
    fn call(function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm, BeamError> {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut heap = Heap::new(64).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["false", "true", "ok", "foo"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        reg.call("erl_debugger", function, arity, &mut ctx, args)
    }

    /// The family is exactly the 13 declared keys, all registered for real.
    #[test]
    fn the_family_is_the_whole_declared_surface() {
        assert_eq!(REAL_BIFS.len(), 13);
        let mut sorted = REAL_BIFS.to_vec();
        sorted.sort_unstable();
        assert!(sorted.iter().eq(REAL_BIFS.iter()), "REAL_BIFS is not sorted");
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(reg.len(), 13, "the erl_debugger key count");
        for &(module, function, arity) in REAL_BIFS {
            assert!(
                reg.contains(module, function, arity),
                "{module}:{function}/{arity} must be registered by this module"
            );
            // …and none of them reads the `RuntimeHooks` seam any more.
            assert!(
                !crate::hook_sets::needs_runtime_hooks(module, function, arity),
                "{module}:{function}/{arity} is a real BIF but still sits in the seam roster"
            );
        }
    }

    /// `supported/0` answers the atom `false` (oracle-verified) and touches no
    /// argument.
    #[test]
    fn supported_reports_the_atom_false() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut heap = Heap::new(64).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["false", "true"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let out = reg
            .call("erl_debugger", "supported", 0, &mut ctx, &[])
            .expect("supported/0 never fails");
        assert!(crate::access::is_atom(out));
        assert_eq!(
            atoms.name(beam_term::atom_val(out)),
            Some("false"),
            "erl_debugger:supported() must be the atom `false`"
        );
    }

    /// The other twelve raise the **bare atom `undef`** — not `badarg` (no argument is
    /// inspected), not the typed `unsupported_bif` gap.
    #[test]
    fn every_tool_raises_the_bare_atom_undef() {
        let args: Vec<Vec<Eterm>> = vec![
            vec![],
            vec![make_small(0)],
            vec![make_small(1)],
            vec![Eterm::default()],
            vec![make_small(0); 4],
        ];
        for &(_, function, arity) in REAL_BIFS {
            if function == "supported" {
                continue;
            }
            let shapes: Vec<&Vec<Eterm>> = args.iter().filter(|a| a.len() as u32 == arity).collect();
            assert!(
                !shapes.is_empty(),
                "no probe argument list of arity {arity} for {function}"
            );
            for shape in shapes {
                match call(function, arity, shape) {
                    Err(BeamError::Raise(raise)) => {
                        assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                        assert_eq!(
                            raise.reason,
                            ExitReason::atom("undef"),
                            "erl_debugger:{function}/{arity} must raise the bare atom undef"
                        );
                    }
                    other => panic!("expected error:undef for {function}/{arity}, got {other:?}"),
                }
            }
        }
        // `whereis/0`/`instrumentations/0` take no argument at all.
        for function in ["whereis", "instrumentations"] {
            match call(function, 0, &[]) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.reason, ExitReason::atom("undef"));
                }
                other => panic!("expected error:undef for {function}/0, got {other:?}"),
            }
        }
    }
}
