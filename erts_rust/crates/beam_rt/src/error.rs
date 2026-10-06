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
//! ([`bad_eval`], [`unsupported`]) follow the convention, and so
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

use beam_common::{BeamError, ExitReason, Raise};
use beam_bifs::BifRegistry;

/// The prefix every capability-gap reason atom starts with (see the module docs).
pub(crate) const UNSUPPORTED_PREFIX: &str = "unsupported";

/// The prefix of the reason atom a runtime **halt** raises (`beam_rt_halt:<status>`).
///
/// A halt is not a capability gap and not a genuine error: it is the runtime
/// terminating with a chosen exit status. It travels out of the executing code as an
/// [`ExceptionClass::Exit`](beam_common::ExceptionClass::Exit) raise carrying this
/// prefix (plus the decimal status), and [`crate::cli::run`] turns
/// [`halt_status`] into the process exit code.
pub(crate) const HALT_PREFIX: &str = "beam_rt_halt";

/// The `erlang:halt/0,1` raise: class `exit`, reason atom `beam_rt_halt:<status>`.
pub(crate) fn halt(status: i32) -> BeamError {
    BeamError::Raise(Raise::exit(ExitReason::atom(format!(
        "{HALT_PREFIX}:{status}"
    ))))
}

/// The exit status carried by a runtime **halt** raise, if `error` is one.
///
/// This is the classifier of the [`HALT_PREFIX`] convention, the counterpart of
/// [`unsupported_reason`]: `Some(status)` for a raise whose reason atom is
/// `beam_rt_halt:<status>`, `None` for anything else.
pub(crate) fn halt_status(error: &BeamError) -> Option<i32> {
    let reason = raise_reason(error)?;
    reason.strip_prefix(HALT_PREFIX)?.strip_prefix(':')?.parse().ok()
}

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

/// A `run` action whose arguments do not match the declared arity.
pub(crate) fn arity_mismatch(spec: &str, expected: u32, found: usize) -> BeamError {
    message(format!(
        "{spec} takes {expected} argument(s), {found} given"
    ))
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

/// A function/arity that the module does not export.
pub(crate) fn unknown_function(module: &str, function: &str, arity: u32) -> BeamError {
    message(format!("{module}:{function}/{arity} is not exported"))
}

/// A call into the `erlang` namespace that is not a registered BIF.
///
/// `erlang` is the BIF namespace and never a loadable module, so an unknown
/// `erlang:*` call is a diagnostic rather than a spurious module load.
pub(crate) fn unknown_bif(module: &str, function: &str, arity: u32) -> BeamError {
    message(format!("{module}:{function}/{arity} is not a BIF"))
}

/// A `run` argument outside the documented term grammar.
pub(crate) fn bad_run_argument(argument: &str) -> BeamError {
    message(format!("{argument:?} is not a valid run argument"))
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

/// The `error:undef` raise of a call that names a function the release does not have.
///
/// `code:is_sticky/2` does not exist in this OTP release, so the oracle raises
/// `error:undef` (verified against OTP 29/erts-17.1:
/// `catch code:is_sticky(kernel,stdlib)` ⇒ `{'EXIT',{undef,…}}`). Reproducing the
/// *absence* honestly means raising the same class and **reason atom** the oracle does
/// (`undef` — the module/function/arity live in the stacktrace, not the reason) rather
/// than inventing a value or reporting a capability gap.
///
/// The reason is the bare atom `undef`, never a message: the differential harness
/// compares reasons exactly.
pub(crate) fn undef() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("undef")))
}

/// A receive-seam operation with no mailbox to serve it.
///
/// The runtime always owns a process table holding the running process, so this is
/// unreachable in Milestone 1; it exists so a context whose pid has no process (a
/// scheduler pass running a terminated pid, say) reports a typed capability gap rather
/// than a wrong result or a panic.
pub(crate) fn no_mailbox(pid: beam_process::ProcessId) -> BeamError {
    unsupported(&format!("receive: process {pid:?} has no mailbox"))
}

/// The `erlang` BIFs that are **known** (they are part of the OTP `erlang` surface) but
/// deliberately not implemented by Milestone-1.
///
/// A call to one of these must never be reported as the *plain* diagnostic
/// [`unknown_bif`] builds (`erlang:X/Y is not a BIF`): that would present a documented
/// capability gap as a genuine runtime error. It is classified as *unsupported* instead
/// (exit code [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED)) — see
/// [`known_unimplemented_bif`].
///
/// This list is the **candidate** set, not the answer: [`is_known_unimplemented_bif`]
/// intersects it with the live `beam_bifs` registry at use time, so a name the registry
/// has since grown is never shadowed and the two can never drift apart (see the
/// [`is_known_unimplemented_bif`] docs). A name in `beam_bifs`' `erlang` registry is
/// therefore dead weight here — remove it when you notice it — but it is *harmless*.
/// The list is curated rather than generated: a name is added when the runtime answers it
/// with a plain `not a BIF` diagnostic and it is in fact an OTP BIF.
pub(crate) const KNOWN_UNIMPLEMENTED_ERLANG_BIFS: &[(&str, u32)] = &[
    ("nodes", 0),
    ("node", 1),
    ("set_cookie", 2),
    ("disconnect_node", 1),
    ("monitor_node", 2),
    ("monitor_node", 3),
    ("port_connect", 2),
    ("port_control", 3),
    ("port_call", 3),
    ("spawn_request", 2),
    ("spawn_request", 3),
    ("spawn_request", 4),
    ("spawn_request", 5),
    ("exit", 2),
    ("link", 1),
    ("unlink", 1),
    ("is_process_alive", 1),
    ("process_flag", 2),
    ("process_flag", 3),
    ("process_info", 1),
    ("process_info", 2),
    ("process_display", 2),
    ("suspend_process", 1),
    ("resume_process", 1),
    ("register", 3),
    ("registered", 0),
    ("system_monitor", 0),
    ("system_monitor", 1),
    ("system_monitor", 2),
    ("system_profile", 0),
    ("system_profile", 1),
    ("system_profile", 2),
    ("start_timer", 3),
    ("start_timer", 4),
    ("cancel_timer", 1),
    ("cancel_timer", 2),
    ("send_after", 3),
    ("send_after", 4),
    ("garbage_collect", 0),
    ("garbage_collect", 1),
    ("garbage_collect", 2),
    ("trace", 3),
    ("trace_pattern", 5),
    ("seq_trace", 1),
    ("seq_trace", 2),
    ("get", 0),
    ("get", 1),
    ("put", 2),
    ("erase", 0),
    ("erase", 1),
    ("apply", 1),
    ("function_exported", 2),
    ("function_exported", 3),
    ("load_module", 2),
    ("load_module", 3),
    ("load_nif", 2),
    ("md5", 1),
    ("md5_init", 0),
    ("md5_update", 2),
    ("md5_final", 1),
    ("system_info", 2),
    ("localtime", 0),
    ("universaltime", 0),
    ("localtime_to_universaltime", 1),
    ("localtime_to_universaltime", 2),
    ("universaltime_to_localtime", 1),
    ("statistics", 0),
    ("statistics", 1),
    ("crc32", 2),
    ("error", 3),
    ("insert_element", 3),
    ("delete_element", 2),
    ("map_get", 3),
    ("concatenate", 2),
];

/// A **known** `erlang` BIF that Milestone-1 deliberately does not implement.
///
/// Reporting it as [`unknown_bif`] would be indistinguishable from a typo (`erlang:nope/1`)
/// and, worse, would classify a documented capability gap as a genuine runtime error. The
/// reason atom therefore starts with `unsupported`, so
/// [`unsupported_reason`](crate::error::unsupported_reason) recognises it and
/// [`cli::run`](crate::cli::run) exits with
/// [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED) — exactly like the `beam_engine`
/// `unsupported_opcode:` and `beam_bifs` `unsupported_bif` gaps.
pub(crate) fn known_unimplemented_bif(function: &str, arity: u32) -> BeamError {
    unsupported(&format!("erlang:{function}/{arity} is a known BIF that is not implemented"))
}

/// Whether `(function, arity)` is **listed** as a known-but-unimplemented `erlang` BIF in
/// [`KNOWN_UNIMPLEMENTED_ERLANG_BIFS`].
///
/// This is the raw list membership; the question the dispatcher asks is
/// [`is_known_unimplemented_bif`], which additionally requires that the live registry does
/// not already own the name.
pub(crate) fn listed_as_unimplemented(function: &str, arity: u32) -> bool {
    KNOWN_UNIMPLEMENTED_ERLANG_BIFS.contains(&(function, arity))
}

/// Whether `(function, arity)` is an **effective** known-but-unimplemented `erlang` BIF —
/// listed in [`KNOWN_UNIMPLEMENTED_ERLANG_BIFS`] **and** not owned by `registry`.
///
/// The effective set is the intersection with the live `beam_bifs` registry rather than the
/// static list alone, so the two can never drift: if a later `beam_bifs` change registers a
/// name the list still carries, the registry wins the dispatch (it is consulted first
/// anyway) and the name stops being classified as a gap automatically. Only a name that is
/// actually served by a plain `erlang:X/Y is not a BIF` diagnostic is ever called a gap.
pub(crate) fn is_known_unimplemented_bif(
    registry: &BifRegistry,
    function: &str,
    arity: u32,
) -> bool {
    listed_as_unimplemented(function, arity) && !registry.contains("erlang", function, arity)
}

/// A monitor whose watching process is not in the process table.
///
/// A context running a pid the table does not hold (a scheduler pass over a terminated
/// pid, say) cannot record a monitor: it reports a typed capability gap rather than
/// silently dropping the monitor or inventing a watcher.
pub(crate) fn no_process(pid: beam_process::ProcessId) -> BeamError {
    unsupported(&format!(
        "erlang:monitor/2: watching process {pid:?} has no process"
    ))
}

/// A scheduler pass asked to run a pid the runtime never spawned an engine for.
///
/// [`Runtime::spawn_process`](crate::runtime::Runtime::spawn_process) registers an entry
/// point for every process it creates and
/// [`Runtime::enqueue_process`](crate::runtime::Runtime::enqueue_process) refuses a pid
/// without one, so this is a programming error in a caller driving the scheduler by hand;
/// it is reported rather than run as a wrong (or empty) function.
pub(crate) fn unknown_process_entry(pid: beam_process::ProcessId) -> BeamError {
    message(format!("scheduler: process {pid:?} has no engine entry point"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_errors_render_their_text() {
        let error = unsupported("operator /");
        assert_eq!(
            error.to_string(),
            "error unsupported: operator /"
        );
        assert!(error.is_raise());
    }

    #[test]
    fn capability_gaps_are_classified_and_stripped() {
        // The constructors here build owned atoms (`unsupported: …`).
        assert_eq!(
            unsupported_reason(&unsupported("unknown switch \"--nope\"")).as_deref(),
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
        assert_eq!(unsupported_reason(&unexpected_argument("junk")), None);
        assert_eq!(unsupported_reason(&bad_spec("slice-add-2")), None);
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
    fn undef_is_the_bare_reason_atom_and_not_a_capability_gap() {
        // The oracle raises `error:undef` (reason atom `undef`) for a function the
        // release does not have, and the differential harness compares reasons exactly.
        let error = undef();
        assert!(error.is_raise());
        assert_eq!(error.as_atom(), Some("undef"));
        assert_eq!(error.to_string(), "error undef");
        // It is a genuine error, not a Milestone-1 capability gap.
        assert_eq!(unsupported_reason(&error), None);
        assert_eq!(halt_status(&error), None);
    }

    #[test]
    fn a_halt_raise_carries_its_status_and_is_not_a_capability_gap() {
        for status in [0, 1, 42, 255] {
            let error = halt(status);
            assert_eq!(halt_status(&error), Some(status), "{error}");
            assert_eq!(unsupported_reason(&error), None, "{error}");
        }
        // A non-halt raise (and a non-raise) is not a halt.
        assert_eq!(halt_status(&unsupported("nope")), None);
        assert_eq!(halt_status(&BeamError::BadArg), None);
        assert_eq!(halt_status(&missing_value("-eval")), None);
    }

    #[test]
    fn diagnostics_name_the_offending_item() {
        assert!(unknown_function("slice", "add", 3)
            .to_string()
            .contains("slice:add/3"));
        assert!(unknown_bif("erlang", "nope", 1)
            .to_string()
            .contains("erlang:nope/1"));
        assert!(bad_run_argument("1+").to_string().contains("1+"));
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
    }

    #[test]
    fn known_unimplemented_bifs_are_capability_gaps_and_never_shadow_the_registry() {
        let registry = beam_bifs::default_registry();
        // A curated entry is a documented capability gap, classified by the same
        // convention as every other `unsupported*` raise (so the CLI exits 3), not the
        // plain `is not a BIF` diagnostic a typo gets.
        assert!(listed_as_unimplemented("nodes", 0));
        assert!(is_known_unimplemented_bif(&registry, "nodes", 0));
        assert!(unsupported_reason(&known_unimplemented_bif("nodes", 0)).is_some());
        assert!(
            unsupported_reason(&known_unimplemented_bif("nodes", 0))
                .expect("a capability gap")
                .contains("erlang:nodes/0")
        );
        // An arity that is not curated is not a known gap.
        assert!(!listed_as_unimplemented("nodes", 1));
        assert!(!is_known_unimplemented_bif(&registry, "nodes", 1));
        assert!(!is_known_unimplemented_bif(&registry, "nope", 1));
    }

    #[test]
    fn the_spawn_family_and_port_command_are_no_longer_curated_gaps() {
        // The native floor serves `spawn/1,2,3`, `spawn_link/1,2,3`, `spawn_opt/2,3,4`
        // and the runtime's port provider serves `port_command/2,3`. Keeping any of them
        // in the curated list would be dead weight (they are answered before the
        // `erlang`-namespace fallback is reached at all) and the `/4` arities OTP does
        // not have must not be presented as a documented capability gap either.
        for (function, arity) in [
            ("spawn", 1u32),
            ("spawn", 2),
            ("spawn", 3),
            ("spawn", 4),
            ("spawn_link", 1),
            ("spawn_link", 2),
            ("spawn_link", 3),
            ("spawn_link", 4),
            ("spawn_opt", 2),
            ("spawn_opt", 3),
            ("spawn_opt", 4),
            ("port_command", 2),
            ("port_command", 3),
        ] {
            assert!(
                !listed_as_unimplemented(function, arity),
                "erlang:{function}/{arity} must no longer be a curated gap"
            );
        }
    }

    #[test]
    fn the_effective_gap_set_never_shadows_the_registry() {
        // The **effective** gap set — the curated list intersected with what the live
        // registry does *not* own — must be disjoint from the registry. That is the
        // drift guard: a `beam_bifs` change that registers a name the static list still
        // carries can never turn a served BIF into an `unsupported` capability gap,
        // because the intersection drops it.
        let registry = beam_bifs::default_registry();
        for (module, function, arity) in registry.names() {
            assert!(
                !(module == "erlang" && is_known_unimplemented_bif(&registry, &function, arity)),
                "erlang:{function}/{arity} is a registered BIF and must not be classified as an unimplemented gap"
            );
        }
        // The guard is not vacuous: a name the registry does *not* own that is on the list
        // still classifies as a gap …
        assert!(!registry.contains("erlang", "nodes", 0));
        assert!(is_known_unimplemented_bif(&registry, "nodes", 0));
        // … and a listed name the registry *has* since grown (the drift this intersection
        // exists to absorb) is a registered BIF, never a gap.
        for grown in [("phash", 2), ("phash2", 1), ("phash2", 2)] {
            assert!(
                registry.contains("erlang", grown.0, grown.1),
                "erlang:{}/{} must be registered",
                grown.0,
                grown.1
            );
            assert!(
                !is_known_unimplemented_bif(&registry, grown.0, grown.1),
                "erlang:{}/{} is registered and must not be a gap",
                grown.0,
                grown.1
            );
        }
    }
}
