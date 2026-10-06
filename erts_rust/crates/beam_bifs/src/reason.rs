//! The `erlang` raise helpers (`error/1`, `error/2`, `exit/1`, `throw/1`,
//! `nif_error/1,2`) and the [`ExitReason`] ↔ reason-term mapping.
//!
//! Ground truth (read-only): `bif erlang:error/1`, `error/2`, `exit/1`,
//! `throw/1` and `nif_error/1,2` in `../../erts/emulator/beam/bif.tab`
//! (lines 65, 149, 284–285, 568) and the well-known reason atoms in
//! `beam_common::ExitReason`. The `nif_error/2` implementation is `nif_error_2`
//! in `../../erts/emulator/beam/bif.c:1525` (the second argument is metadata for
//! the crash report, **not** the reason).
//!
//! # Return-based raising
//!
//! A raise is a *value*, not a `longjmp` (workspace `CONTEXT.md` §3): the BIFs
//! return `Err(BeamError::Raise(..))` and the engine installs the exception tag in
//! `x(0)`. `error/1`, `exit/1` and `throw/1` set the [`ExceptionClass`] to
//! `Error`, `Exit` and `Throw` respectively.
//!
//! # The documented `Structured` gap
//!
//! [`ExitReason`] models the well-known atoms as dedicated variants and any other
//! atom as `Atom(name)`, but a **non-atom** reason is `Structured` and *carries no
//! term* — `beam_common` has no term dependency, so the live term stays owned by
//! the term heap (the engine / `beam_rt` capture it at the raise site). The
//! mapping is therefore lossy in exactly that one direction:
//! [`reason_term_from_exit_reason`] rejects `Structured` with `badarg`. This is
//! the handoff `../beam_process/CONTEXT.md` documents.
//!
//! Both [`exit_reason_from_term`] and [`reason_term_from_exit_reason`] are part of
//! the crate's public API (re-exported from the crate root) — the documented
//! `beam_process`/`beam_rt` handoff, not crate-internal helpers.

use beam_common::{BeamError, ExitReason, Result};
use beam_term::{atom_val, make_atom, AtomTable, Eterm};

use crate::context::BifContext;
use crate::error::{raise_error, raise_exit, raise_throw};
use crate::registry::BifRegistry;

/// Maps a live reason **term** to an [`ExitReason`].
///
/// An atom maps to its dedicated [`ExitReason`] variant when well-known
/// (`normal`, `shutdown`, `noproc`, `noconnection`, `killed`, `aborted`) and to
/// `Atom(name)` otherwise; a non-atom maps to [`ExitReason::Structured`].
///
/// An atom whose name is absent from `atoms` (a mismatched table) also maps to
/// `Structured`: `ExitReason::Atom` requires a name and there is none to attach
/// (see the module docs).
pub fn exit_reason_from_term(term: Eterm, atoms: &AtomTable) -> ExitReason {
    if !beam_term::is_atom(term) {
        return ExitReason::Structured;
    }
    match atoms.name(atom_val(term)) {
        Some("normal") => ExitReason::Normal,
        Some("shutdown") => ExitReason::Shutdown,
        Some("noproc") => ExitReason::NoProc,
        Some("noconnection") => ExitReason::NoConnection,
        Some("killed") => ExitReason::Killed,
        Some("aborted") => ExitReason::Aborted,
        Some(name) => ExitReason::Atom(name.to_string().into()),
        None => ExitReason::Structured,
    }
}

/// Maps an [`ExitReason`] back to its interned reason **term**.
///
/// # Errors
///
/// [`BeamError::BadArg`] for [`ExitReason::Structured`] (and any future
/// non-exhaustive variant): it carries no term to reproduce (see the module docs).
pub fn reason_term_from_exit_reason(
    reason: &ExitReason,
    atoms: &mut AtomTable,
) -> Result<Eterm> {
    match reason {
        ExitReason::Normal => Ok(make_atom(atoms.intern("normal"))),
        ExitReason::Shutdown => Ok(make_atom(atoms.intern("shutdown"))),
        ExitReason::NoProc => Ok(make_atom(atoms.intern("noproc"))),
        ExitReason::NoConnection => Ok(make_atom(atoms.intern("noconnection"))),
        ExitReason::Killed => Ok(make_atom(atoms.intern("killed"))),
        ExitReason::Aborted => Ok(make_atom(atoms.intern("aborted"))),
        ExitReason::Atom(name) => Ok(make_atom(atoms.intern(name.as_str()))),
        _ => Err(BeamError::BadArg),
    }
}

/// `erlang:error/1`.
fn error_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise_error(exit_reason_from_term(args[0], ctx.atoms())))
}

/// `erlang:error/2` — the argument list is ignored in M1 (documented simplification).
fn error_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise_error(exit_reason_from_term(args[0], ctx.atoms())))
}

/// `erlang:exit/1`.
fn exit_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise_exit(exit_reason_from_term(args[0], ctx.atoms())))
}

/// `erlang:throw/1`.
fn throw_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise_throw(exit_reason_from_term(args[0], ctx.atoms())))
}

/// `erlang:nif_error/1` — a NIF raises `error` with its argument as the reason.
///
/// The oracle (erts-17.1) raises the class `error` with the argument *unchanged*:
/// `nif_error(foo)` is `error:foo`, `nif_error(undefined)` is `error:undefined`
/// and `nif_error({nif_not_loaded,m,f,0})` is `error:{nif_not_loaded,m,f,0}`.
/// The non-atom case is the documented [`ExitReason::Structured`] gap — the live
/// term is preserved by the engine at the raise site (`beam_rt`'s term-carrying
/// `Flow::Raise`), not by this BIF.
fn nif_error_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise_error(exit_reason_from_term(args[0], ctx.atoms())))
}

/// `erlang:nif_error/2` — as `nif_error/1`; the second argument is metadata.
///
/// `nif_error_2` in `../../erts/emulator/beam/bif.c:1525` raises with the
/// **first** argument as the reason and lets the second (the argument list the
/// crash report prints) fall away: `nif_error(foo, [1,2])` is `error:foo` and
/// `nif_error(true, [1,2])` is `error:true`.
fn nif_error_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise_error(exit_reason_from_term(args[0], ctx.atoms())))
}

/// Registers `erlang:error/1,2`, `exit/1`, `throw/1` and `nif_error/1,2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "error", 1, error_1);
    reg.register("erlang", "error", 2, error_2);
    reg.register("erlang", "exit", 1, exit_1);
    reg.register("erlang", "throw", 1, throw_1);
    reg.register("erlang", "nif_error", 1, nif_error_1);
    reg.register("erlang", "nif_error", 2, nif_error_2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::ExceptionClass;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn seeded() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "hello",
            "normal",
            "shutdown",
            "foo",
            "undefined",
            "nif_not_loaded",
            "m",
            "f",
        ]);
        atoms
    }

    /// Runs a raise BIF and returns its `(class, reason)`.
    fn raise_of(
        f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
        arg: Eterm,
    ) -> (ExceptionClass, ExitReason) {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match f(&mut ctx, &[arg]) {
            Err(BeamError::Raise(raise)) => (raise.class, raise.reason),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    /// Runs a two-argument raise BIF and returns its `(class, reason)`.
    fn raise_of2(
        f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
        first: Eterm,
        second: Eterm,
    ) -> (ExceptionClass, ExitReason) {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match f(&mut ctx, &[first, second]) {
            Err(BeamError::Raise(raise)) => (raise.class, raise.reason),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    #[test]
    fn error_exit_and_throw_set_the_class() {
        let mut heap = Heap::new(16).unwrap();
        let atoms = seeded();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let hello = ctx.atom("hello").unwrap();

        assert_eq!(
            raise_of(error_1, hello),
            (ExceptionClass::Error, ExitReason::atom("hello"))
        );
        assert_eq!(
            raise_of(exit_1, hello),
            (ExceptionClass::Exit, ExitReason::atom("hello"))
        );
        assert_eq!(
            raise_of(throw_1, hello),
            (ExceptionClass::Throw, ExitReason::atom("hello"))
        );
        // `error/2` behaves like `error/1` (ignoring the argument list).
        assert_eq!(
            raise_of(error_2, hello),
            (ExceptionClass::Error, ExitReason::atom("hello"))
        );
    }

    #[test]
    fn well_known_atoms_map_to_their_variants() {
        let mut heap = Heap::new(16).unwrap();
        let atoms = seeded();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let normal = ctx.atom("normal").unwrap();
        let shutdown = ctx.atom("shutdown").unwrap();
        assert_eq!(raise_of(exit_1, normal), (ExceptionClass::Exit, ExitReason::Normal));
        assert_eq!(
            raise_of(exit_1, shutdown),
            (ExceptionClass::Exit, ExitReason::Shutdown)
        );
    }

    #[test]
    fn non_atom_reasons_are_structured() {
        assert_eq!(
            raise_of(throw_1, make_small(42)),
            (ExceptionClass::Throw, ExitReason::Structured)
        );
        assert_eq!(
            raise_of(error_1, NIL),
            (ExceptionClass::Error, ExitReason::Structured)
        );
    }

    /// Oracle-pinned (`erl -eval 'erlang:nif_error(T)'`): the class is always
    /// `error` and the reason is the first argument unchanged.
    #[test]
    fn nif_error_1_raises_error_with_the_argument_as_reason() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let foo = ctx.atom("foo").expect("seeded");
        let undefined = ctx.atom("undefined").expect("seeded");
        let tru = ctx.atom("true").expect("seeded");

        // `erlang:nif_error(foo)` ⇒ `error:foo`.
        assert_eq!(
            raise_of(nif_error_1, foo),
            (ExceptionClass::Error, ExitReason::atom("foo"))
        );
        // `erlang:nif_error(undefined)` ⇒ `error:undefined`.
        assert_eq!(
            raise_of(nif_error_1, undefined),
            (ExceptionClass::Error, ExitReason::atom("undefined"))
        );
        assert_eq!(
            raise_of(nif_error_1, tru),
            (ExceptionClass::Error, ExitReason::atom("true"))
        );

        // `erlang:nif_error({nif_not_loaded,m,f,0})` ⇒ `error:{nif_not_loaded,m,f,0}`.
        // A compound reason is the documented `Structured` gap: the BIF reports the
        // class and `Structured`, and the engine preserves the live term at the
        // raise site (`beam_rt`'s term-carrying `Flow::Raise`).
        let nif_not_loaded = ctx.atom("nif_not_loaded").expect("seeded");
        let m = ctx.atom("m").expect("seeded");
        let f = ctx.atom("f").expect("seeded");
        let tuple = ctx
            .tuple(&[nif_not_loaded, m, f, make_small(0)])
            .expect("fits");
        assert_eq!(
            raise_of(nif_error_1, tuple),
            (ExceptionClass::Error, ExitReason::Structured)
        );
    }

    /// `erlang:nif_error/2` uses the **first** argument as the reason; the second
    /// (the crash-report argument list) is metadata (`bif.c:1525`, `nif_error_2`).
    #[test]
    fn nif_error_2_uses_the_first_argument_as_the_reason() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let foo = ctx.atom("foo").expect("seeded");
        let tru = ctx.atom("true").expect("seeded");
        let meta = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

        // `erlang:nif_error(foo, [1,2])` ⇒ `error:foo`.
        assert_eq!(
            raise_of2(nif_error_2, foo, meta),
            (ExceptionClass::Error, ExitReason::atom("foo"))
        );
        // `erlang:nif_error(true, [1,2])` ⇒ `error:true`.
        assert_eq!(
            raise_of2(nif_error_2, tru, meta),
            (ExceptionClass::Error, ExitReason::atom("true"))
        );
    }

    #[test]
    fn exit_reason_from_term_maps_atoms_and_structures() {
        let mut atoms = seeded();
        let normal = make_atom(atoms.intern("normal"));
        let shutdown = make_atom(atoms.intern("shutdown"));
        let noproc = make_atom(atoms.intern("noproc"));
        let noconnection = make_atom(atoms.intern("noconnection"));
        let killed = make_atom(atoms.intern("killed"));
        let aborted = make_atom(atoms.intern("aborted"));
        let hello = make_atom(atoms.intern("hello"));

        assert_eq!(exit_reason_from_term(normal, &atoms), ExitReason::Normal);
        assert_eq!(exit_reason_from_term(shutdown, &atoms), ExitReason::Shutdown);
        assert_eq!(exit_reason_from_term(noproc, &atoms), ExitReason::NoProc);
        assert_eq!(exit_reason_from_term(noconnection, &atoms), ExitReason::NoConnection);
        assert_eq!(exit_reason_from_term(killed, &atoms), ExitReason::Killed);
        assert_eq!(exit_reason_from_term(aborted, &atoms), ExitReason::Aborted);
        assert_eq!(exit_reason_from_term(hello, &atoms), ExitReason::atom("hello"));

        // Non-atom reasons, including the empty list and integers.
        assert_eq!(exit_reason_from_term(NIL, &atoms), ExitReason::Structured);
        assert_eq!(exit_reason_from_term(make_small(1), &atoms), ExitReason::Structured);

        // A pid term is not an atom either.
        assert_eq!(
            exit_reason_from_term(ProcessId::new(1, 0).to_eterm(), &atoms),
            ExitReason::Structured
        );
    }

    #[test]
    fn reason_term_from_exit_reason_round_trips_atoms() {
        let mut atoms = seeded();
        for reason in [
            ExitReason::Normal,
            ExitReason::Shutdown,
            ExitReason::NoProc,
            ExitReason::NoConnection,
            ExitReason::Killed,
            ExitReason::Aborted,
            ExitReason::atom("hello"),
        ] {
            let term = reason_term_from_exit_reason(&reason, &mut atoms).unwrap();
            assert!(beam_term::is_atom(term));
            assert_eq!(exit_reason_from_term(term, &atoms), reason);
        }
    }

    #[test]
    fn reason_term_from_exit_reason_rejects_structured() {
        let mut atoms = seeded();
        assert!(matches!(
            reason_term_from_exit_reason(&ExitReason::Structured, &mut atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn register_installs_the_raise_helpers() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "error", 1));
        assert!(reg.contains("erlang", "error", 2));
        assert!(reg.contains("erlang", "exit", 1));
        assert!(reg.contains("erlang", "throw", 1));
        assert!(reg.contains("erlang", "nif_error", 1));
        assert!(reg.contains("erlang", "nif_error", 2));
        assert_eq!(reg.len(), 6);
    }
}
