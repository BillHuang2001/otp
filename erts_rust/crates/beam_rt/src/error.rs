//! The runtime's `pub(crate)` [`BeamError`] constructors.
//!
//! `beam_rt` owns **no crate-local error enum** (workspace `CONTEXT.md` §3): every
//! diagnostic is a [`beam_common::BeamError`]. The message-carrying constructor
//! [`message`] builds an `error`-class raise whose reason is an atom holding the
//! text, which is exactly how the CLI surfaces a diagnostic — [`BeamError`]
//! implements [`std::fmt::Display`], and that renders the message.
//!
//! The single exception is an unknown `system_info/1` key: the OTP oracle raises
//! the bare atom `badarg` (verified against erts-17.1), so [`unknown_info_key`]
//! returns [`BeamError::BadArg`] and nothing more.
//!
//! # Capability gaps (the "unsupported" convention)
//!
//! A *capability gap* — a feature Milestone-1 deliberately does not implement, as
//! opposed to a genuine failure or a malformed invocation — is signalled by a raise
//! whose **reason atom starts with `unsupported`**. The constructors here
//! ([`unknown_switch`], [`bad_eval`], [`unsupported`]) follow the convention, and so
//! do the sibling crates: `beam_engine` raises `unsupported_opcode: <what>` and
//! `beam_bifs` raises `unsupported_bif`.
//!
//! [`unsupported_reason`] is the **single** place that convention is interpreted: it
//! classifies any such raise and returns the human-readable reason. The CLI maps
//! `Some(_)` from it onto the stable external signal — the single stderr line
//! `beam_rt: unsupported: <reason>` and exit code
//! [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED) — which the differential harness
//! keys off. Classification must go through [`unsupported_reason`] rather than
//! [`BeamError::as_atom`], because the latter returns `None` for an
//! [`ExitReason::Atom`](beam_common::ExitReason::Atom) backed by an owned `String`
//! (which every message here is).

use std::path::PathBuf;

use beam_common::{BeamError, ExitReason, Raise};

/// The prefix every capability-gap reason atom starts with (see the module docs).
pub(crate) const UNSUPPORTED_PREFIX: &str = "unsupported";

/// A message-carrying `error`-class raise — the runtime's general diagnostic.
pub(crate) fn message(text: impl Into<String>) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(text.into())))
}

/// The reason of a [`BeamError::Raise`], as an atom name, when there is one.
///
/// Reads through [`ExitReason::as_atom`], which — unlike
/// [`BeamError::as_atom`] — also sees atoms backed by an owned `String` (every
/// message this module builds). Every other error is `None`.
fn raise_reason(error: &BeamError) -> Option<&str> {
    match error {
        BeamError::Raise(raise) => raise.reason.as_atom(),
        _ => None,
    }
}

/// Classifies `error` as a **capability gap**, returning the human-readable reason.
///
/// Returns `Some(reason)` iff `error` is a raise whose reason atom starts with
/// [`UNSUPPORTED_PREFIX`]; the reason is that atom with the leading `unsupported`
/// token and any following `:`, `_` or whitespace stripped — so `unsupported: X`
/// becomes `X`, `unsupported_opcode: Y` becomes `opcode: Y` and `unsupported_bif`
/// becomes `bif`.
///
/// This is the single interpretation of the convention (see the module docs): the
/// CLI turns `Some(_)` into the `beam_rt: unsupported: <reason>` stderr line and exit
/// code [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED). A `None` means the error
/// is a genuine runtime error or a usage error, classified by the caller as usual.
pub(crate) fn unsupported_reason(error: &BeamError) -> Option<String> {
    let reason = raise_reason(error)?;
    let rest = reason.strip_prefix(UNSUPPORTED_PREFIX)?;
    Some(
        rest.trim_start_matches(|c: char| c == ':' || c == '_' || c.is_whitespace())
            .to_string(),
    )
}

/// An unrecognized command-line switch.
///
/// A switch outside the documented A.6 surface is a **capability gap**, not a usage
/// error: real `erl` treats an unmodeled switch as an init argument (verified:
/// `erl -noshell --nope -eval 'halt(0).'` exits 0), so it is classified as
/// *unsupported* and named in the message (contract A.6: never silently ignored).
pub(crate) fn unknown_switch(switch: &str) -> BeamError {
    unsupported(&format!("unknown switch {switch:?}"))
}

/// A malformed value of a *recognized* switch (e.g. `+P x`).
pub(crate) fn bad_switch_value(switch: &str, value: &str) -> BeamError {
    message(format!("bad value {value:?} for switch {switch}"))
}

/// A recognized switch whose required value is missing (e.g. a trailing `-eval`).
pub(crate) fn missing_value(switch: &str) -> BeamError {
    message(format!("switch {switch} requires a value"))
}

/// A command-line argument that is neither a switch nor part of an action.
pub(crate) fn unexpected_argument(argument: &str) -> BeamError {
    message(format!("unexpected argument {argument:?}"))
}

/// A malformed `run` target spec (expected `module:function/arity`).
pub(crate) fn bad_spec(spec: &str) -> BeamError {
    message(format!(
        "{spec:?} is not a valid module:function/arity spec"
    ))
}

/// A `run` action whose integer arguments do not match the declared arity.
pub(crate) fn arity_mismatch(spec: &str, expected: u32, found: usize) -> BeamError {
    message(format!(
        "{spec} takes {expected} integer argument(s), {found} given"
    ))
}

/// An atom argument that is not interned in the target module's atom table.
pub(crate) fn unknown_atom(module: &str, name: &str) -> BeamError {
    message(format!("atom {name:?} is not interned in module {module:?}"))
}
/// An unknown `system_info/1` key — the bare `badarg` the oracle raises.
///
/// The key is deliberately *not* carried: the oracle's exception reason is exactly
/// the atom `badarg` (`erlang:system_info(nonsense)` ⇒ `error:badarg`), and the
/// differential harness compares reasons.
pub(crate) fn unknown_info_key(_key: &str) -> BeamError {
    BeamError::BadArg
}

/// A module that is not loaded and has no cached `.beam`.
pub(crate) fn unknown_module(module: &str) -> BeamError {
    message(format!("module {module:?} is not loaded"))
}

/// A module whose `<module>.beam` is not in the configured code path.
pub(crate) fn module_not_found(module: &str, paths: &[PathBuf]) -> BeamError {
    let searched: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    message(format!(
        "cannot find {module}.beam in the code path [{}]",
        searched.join(", ")
    ))
}

/// A function/arity that the module does not export.
pub(crate) fn unknown_function(module: &str, function: &str, arity: u32) -> BeamError {
    message(format!("{module}:{function}/{arity} is not exported"))
}

/// A `-eval` expression outside the supported integer-expression subset.
///
/// The grammar is a documented Milestone-1 capability-gap boundary, so a rejection
/// here is classified as *unsupported* (exit code
/// [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED)) — distinct from a genuine
/// runtime error such as an arithmetic overflow, which comes from the BIF.
pub(crate) fn bad_eval(expr: &str, detail: &str) -> BeamError {
    unsupported(&format!("cannot evaluate {expr:?}: {detail}"))
}

/// An operation the Milestone-1 runtime does not implement.
pub(crate) fn unsupported(what: &str) -> BeamError {
    message(format!("{UNSUPPORTED_PREFIX}: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_errors_render_their_text() {
        let error = unknown_switch("--nope");
        assert_eq!(
            error.to_string(),
            "error unsupported: unknown switch \"--nope\""
        );
        assert!(error.is_raise());
    }

    #[test]
    fn capability_gaps_are_classified_and_stripped() {
        // The constructors here build owned atoms (`unsupported: …`).
        assert_eq!(
            unsupported_reason(&unknown_switch("--nope")).as_deref(),
            Some("unknown switch \"--nope\"")
        );
        assert_eq!(
            unsupported_reason(&bad_eval("1+", "unexpected end")).as_deref(),
            Some("cannot evaluate \"1+\": unexpected end")
        );
        assert_eq!(
            unsupported_reason(&unsupported("operator /")).as_deref(),
            Some("operator /")
        );
        // The engine's `unsupported_opcode: <what>` raise — an OWNED atom, invisible
        // to `BeamError::as_atom`, so this exercises the owned-atom path.
        let engine = BeamError::Raise(Raise::error(ExitReason::atom(String::from(
            "unsupported_opcode: x",
        ))));
        assert_eq!(engine.as_atom(), None);
        assert_eq!(unsupported_reason(&engine).as_deref(), Some("opcode: x"));
        // The BIF registry's bare `unsupported_bif` atom.
        let bif = BeamError::Raise(Raise::error(ExitReason::atom(String::from(
            "unsupported_bif",
        ))));
        assert_eq!(unsupported_reason(&bif).as_deref(), Some("bif"));
    }

    #[test]
    fn genuine_errors_are_not_classified_as_capability_gaps() {
        assert_eq!(unsupported_reason(&missing_value("-eval")), None);
        assert_eq!(unsupported_reason(&module_not_found("slice", &[])), None);
        assert_eq!(unsupported_reason(&unknown_info_key("nonsense")), None);
        assert_eq!(unsupported_reason(&BeamError::BadArg), None);
        assert_eq!(unsupported_reason(&BeamError::SystemLimit), None);
    }
    #[test]
    fn unknown_info_key_is_bare_badarg() {
        assert!(matches!(unknown_info_key("nonsense"), BeamError::BadArg));
        assert_eq!(unknown_info_key("nonsense").as_atom(), Some("badarg"));
    }

    #[test]
    fn diagnostics_name_the_offending_item() {
        assert!(unknown_function("slice", "add", 3)
            .to_string()
            .contains("slice:add/3"));
        assert!(bad_switch_value("+P", "x").to_string().contains("\"x\""));
        assert!(unexpected_argument("junk").to_string().contains("junk"));
        assert!(unknown_module("slice").to_string().contains("slice"));
        assert!(bad_eval("1+", "unexpected end").to_string().contains("1+"));
        assert!(unsupported("preloaded beams")
            .to_string()
            .contains("preloaded beams"));
        assert!(missing_value("-eval").to_string().contains("-eval"));
        assert!(bad_spec("slice-add-2").to_string().contains("slice-add-2"));
        assert!(arity_mismatch("slice:add/2", 2, 1)
            .to_string()
            .contains("slice:add/2"));
        assert!(unknown_atom("slice", "z").to_string().contains("\"z\""));
        let not_found = module_not_found("slice", &[PathBuf::from("/tmp/code")]);
        assert!(not_found.to_string().contains("slice.beam"));
        assert!(not_found.to_string().contains("/tmp/code"));
    }
}
