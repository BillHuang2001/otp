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

use std::path::PathBuf;

use beam_common::{BeamError, ExitReason, Raise};

/// A message-carrying `error`-class raise — the runtime's general diagnostic.
pub(crate) fn message(text: impl Into<String>) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(text.into())))
}

/// An unrecognized command-line switch (contract A.6: never silently ignored).
pub(crate) fn unknown_switch(switch: &str) -> BeamError {
    message(format!("unknown switch {switch:?}"))
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

/// A malformed `-eval` expression.
pub(crate) fn bad_eval(expr: &str, detail: &str) -> BeamError {
    message(format!("cannot evaluate {expr:?}: {detail}"))
}

/// An operation the Milestone-1 runtime does not implement.
pub(crate) fn unsupported(what: &str) -> BeamError {
    message(format!("unsupported: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_errors_render_their_text() {
        let error = unknown_switch("--nope");
        assert_eq!(error.to_string(), "error unknown switch \"--nope\"");
        assert!(error.is_raise());
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
