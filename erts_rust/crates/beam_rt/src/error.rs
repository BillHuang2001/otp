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

use beam_bifs::BifRegistry;
use beam_common::{AllocKind, BeamError, ExitReason, LiveReason, Raise};
use beam_term::{atom_val, is_atom, AtomTable, Eterm};

/// Turns a **term-heap exhaustion** into the emulator's documented hard limit.
///
/// Two exhaustion classes reach this translation, and both mean *the destination heap could
/// not hold the term* — a soft, recoverable signal the caller should collect-and-retry, **not**
/// a fatal host-level OOM:
///
/// * [`AllocKind::Heap`] — `beam_alloc`'s per-process heap bump allocator (`beam_alloc` crate
///   `CONTEXT.md`, Design Decisions);
/// * [`AllocKind::Message`] — the copy engine's destination-heap exhaustion
///   ([`beam_message::is_message_heap_exhaustion`]), raised by `copy_struct` and its siblings
///   when the receiver's message heap cannot hold the copied term.
///
/// This runtime has no collector to retry with, so an exhausted term heap surfaces as the
/// emulator's `error:system_limit` reason — [`crate::cli`] reports it as
/// `beam_rt: system_limit` with [`EXIT_RUNTIME`](crate::cli::EXIT_RUNTIME). (The C does *not*
/// raise `system_limit` when a process reaches its `max_heap_size` (`+hmax`): it kills the
/// process with reason `killed` and may log an error report — `erts/emulator/beam/erl_gc.c:1208`.
/// `system_limit` is this runtime's own honest reason for the retry it cannot perform.)
///
/// Reporting the raw [`BeamError::Alloc`] instead would print the host-level
/// `allocation failure: heap (N bytes)` / `allocation failure: message (N bytes)` diagnostic,
/// which describes the *internal* allocator state of this implementation, not a BEAM error a
/// program (or the differential harness) can reason about.
///
/// Every other error is returned unchanged, so this is a pure translation — nothing is
/// swallowed.
///
/// # Ordering at the message-delivery seam
///
/// The *oversized-message* fallback keys on the **raw** [`AllocKind::Message`] error to decide
/// to retry through the heap-fragment model
/// ([`RuntimeContext::deliver_message`](crate::context::RuntimeContext)), so this translation
/// must be applied only **after** that retry has run — never to the first `deliver_simple`
/// result. Translating earlier would turn the retry trigger into a fatal `system_limit` and
/// break every oversized delivery.
pub(crate) fn enforce_term_heap_limit(error: BeamError) -> BeamError {
    let exhausted = matches!(&error, BeamError::Alloc(failure) if failure.kind == AllocKind::Heap)
        || beam_message::is_message_heap_exhaustion(&error);
    if exhausted {
        BeamError::SystemLimit
    } else {
        error
    }
}
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

/// The prefix of the reason atom an **abort** raises (`beam_rt_abort`).
///
/// `erlang:halt(abort)` and `erlang:halt(abort, Options)` terminate the runtime with
/// `SIGABRT` in the oracle (`erts_exit(ERTS_ABORT_EXIT, "")` -> `abort()`,
/// `erts/emulator/beam/erl_init.c:2774-2775`), which a shell observes as exit code `134`.
/// Like [`halt`] the abort travels out of the executing code as an `exit`-class raise; the
/// CLI recognises it with [`is_abort`] and calls [`std::process::abort`], so the host really
/// receives the signal instead of a plain status.
pub(crate) const ABORT_PREFIX: &str = "beam_rt_abort";

/// The `erlang:halt(abort)` raise: class `exit`, reason atom `beam_rt_abort`.
pub(crate) fn abort() -> BeamError {
    BeamError::Raise(Raise::exit(ExitReason::atom(ABORT_PREFIX)))
}

/// Whether `error` is a runtime **abort** raise (the classifier of [`ABORT_PREFIX`]).
pub(crate) fn is_abort(error: &BeamError) -> bool {
    raise_reason(error) == Some(ABORT_PREFIX)
}

/// The prefix of the reason atom a **crash-dump slogan** halt raises
/// (`beam_rt_dump:<slogan>`).
///
/// `erlang:halt("Slogan")` (a string or `[]`) terminates with `ERTS_DUMP_EXIT`
/// (`erts/emulator/beam/bif.c:4393`): the slogan is printed on stderr, a crash dump is
/// written and the process exits `1`. The slogan may be arbitrary and multi-line, but the
/// raise reason is a single atom, so the caller strips CR/LF before constructing it (see
/// [`crate::native`]).
pub(crate) const DUMP_PREFIX: &str = "beam_rt_dump";

/// The `erlang:halt("Slogan")` raise: class `exit`, reason atom `beam_rt_dump:<slogan>`.
pub(crate) fn dump(slogan: &str) -> BeamError {
    BeamError::Raise(Raise::exit(ExitReason::atom(format!(
        "{DUMP_PREFIX}:{slogan}"
    ))))
}

/// The dump slogan carried by a dump-slogan raise, if `error` is one (the classifier of
/// [`DUMP_PREFIX`]). `Some("")` for `halt([])`.
pub(crate) fn dump_slogan(error: &BeamError) -> Option<&str> {
    raise_reason(error)?
        .strip_prefix(DUMP_PREFIX)?
        .strip_prefix(':')
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
pub fn unsupported_reason(error: &BeamError) -> Option<String> {
    let reason = raise_reason(error)?;
    let rest = reason.strip_prefix(UNSUPPORTED_PREFIX)?;
    Some(
        rest.trim_start_matches(|c: char| c == ':' || c == '_' || c.is_whitespace())
            .to_string(),
    )
}

/// Recovers the **atom** reason of an uncaught raise that carries one as a *live term*.
///
/// The engine propagates a raise that escapes every catch frame term-independently: the
/// reason is [`ExitReason::Structured`] and the live reason *term* rides out in the
/// raise's [`Raise::live_reason`] slot (`beam_engine`'s `Machine::throw_with_trace`),
/// which is what lets a genuinely compound reason (`{badkey, K}`, `{case_clause, V}`)
/// survive a boundary that has no term type. For a reason that is a **bare atom** that
/// placeholder simply carries less than the runtime knows — C raises the *atom*
/// (`i_func_info_IaaI` sets `EXC_FUNCTION_CLAUSE` for `function_clause`) — and the name
/// is recoverable from the live term through the shared atom table.
///
/// This function performs exactly that recovery, so an uncaught `function_clause` names
/// itself (`BeamError::as_atom()` answers `Some("function_clause")` and
/// [`Display`](std::fmt::Display) prints `error function_clause`) instead of the
/// `<structured reason>` placeholder. The live reason term is **kept**, so catchability
/// (a live catch frame lands the term) and every live-term consumer
/// ([`crate::eval`], [`crate::runner`]) are unaffected.
///
/// Everything else is returned **unchanged**: a non-raise, a raise that already carries
/// an atom reason, a raise with no live term, and a live term that is not an atom keep
/// [`ExitReason::Structured`] (and `as_atom() == None`). A name the table does not hold is
/// likewise left alone — never a substituted, wrong atom.
///
/// A recovered name that the runtime's own core seed ([`crate::boot::CORE_ATOMS`]) spells
/// gets that **static** name, so [`BeamError::as_atom`] (which hands back a `&'static str`)
/// names it too; a name only the live table knows keeps the owned form, where
/// [`Display`](std::fmt::Display) still names it but `as_atom()` cannot.
pub(crate) fn name_atom_raise(error: BeamError, atoms: &AtomTable) -> BeamError {
    let mut raise = match error {
        BeamError::Raise(raise) => raise,
        other => return other,
    };
    if raise.reason.as_atom().is_some() {
        return BeamError::Raise(raise);
    }
    let name = raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
        .filter(|term| is_atom(*term))
        .and_then(|term| atoms.name(atom_val(term)))
        .map(str::to_string);
    if let Some(name) = name {
        raise.reason = match static_core_atom(&name) {
            Some(known) => ExitReason::atom(known),
            None => ExitReason::atom(name),
        };
    }
    BeamError::Raise(raise)
}

/// The `&'static str` spelling of `name`, when the runtime's core atom seed
/// ([`crate::boot::CORE_ATOMS`]) holds that name.
///
/// The seed is the runtime's own fixed atom vocabulary, so its members are exactly the
/// names that have a static spelling; anything else is known only to the live table.
fn static_core_atom(name: &str) -> Option<&'static str> {
    crate::boot::CORE_ATOMS
        .iter()
        .copied()
        .find(|known| *known == name)
}/// The prefix of the reason atom for a switch value the **emulator/launcher** rejects
/// (`beam_rt_flag:<diagnostic>`).
///
/// A malformed value of a recognized emulator switch (`+t`, `+P`, `+Q`, `+S`, `+hms`,
/// `+hmax`, `+sbt`, …) is neither a CLI usage error nor a capability gap: the oracle's
/// `beam.smp` prints its own one-line diagnostic plus its 7331-byte usage text and exits
/// **1**. This runtime reports the same exit status with its own one-line diagnostic
/// (the `beam.smp` usage block is deliberately not reproduced).
pub(crate) const FLAG_PREFIX: &str = "beam_rt_flag";

/// The emulator/launcher flag rejection: class `error`, reason atom `beam_rt_flag:<diag>`.
pub(crate) fn flag_rejected(diagnostic: impl Into<String>) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!(
        "{FLAG_PREFIX}:{}",
        diagnostic.into()
    ))))
}

/// The diagnostic carried by an emulator-flag rejection, if `error` is one (the classifier
/// of [`FLAG_PREFIX`]). [`crate::cli::run`] prints it and exits **1** (no [`USAGE`] block).
///
/// [`USAGE`]: crate::cli::USAGE
pub(crate) fn flag_rejected_diagnostic(error: &BeamError) -> Option<&str> {
    raise_reason(error)?
        .strip_prefix(FLAG_PREFIX)?
        .strip_prefix(':')
}

/// The prefix of the reason atom for a **boot failure caused by an unreadable `-config`
/// file** (`beam_rt_config:<resolved path>`).
///
/// The oracle's boot starts the node and then fails while `application_controller` reads
/// the file: it prints `Could not start kernel pid (application_controller) (...)` on
/// stderr, writes a real crash dump and exits **1**. The resolved path is carried here so
/// the CLI can reproduce that text byte-for-byte.
pub(crate) const CONFIG_PREFIX: &str = "beam_rt_config";

/// The unreadable-`-config` boot failure raise.
pub(crate) fn config_boot_failure(path: &std::path::Path) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!(
        "{CONFIG_PREFIX}:{}",
        path.display()
    ))))
}

/// The resolved `-config` path carried by a config boot failure (the classifier of
/// [`CONFIG_PREFIX`]).
pub(crate) fn config_failure_path(error: &BeamError) -> Option<&str> {
    raise_reason(error)?
        .strip_prefix(CONFIG_PREFIX)?
        .strip_prefix(':')
}

/// A malformed value of a *recognized* switch (e.g. `+P x`).
///
/// A malformed value of a recognized switch is rejected by the **emulator** (or by the
/// `erl` launcher, for its own switches such as `-env`/`-emu_flavor`): the oracle prints
/// its own diagnostic and exits **1**, so this is a [`flag_rejected`] raise — *not* the
/// CLI's usage status 2. [`crate::cli::run`] classifies it with
/// [`flag_rejected_diagnostic`] and prints one line, without a [`USAGE`] block.
///
/// [`USAGE`]: crate::cli::USAGE
pub(crate) fn bad_switch_value(switch: &str, value: &str) -> BeamError {
    flag_rejected(format!("bad value {value:?} for switch {switch}"))
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

/// The live **structured reason term** carried by a raise, when it has one.
///
/// A reason that is not an atom travels out of band: `beam_common`'s
/// [`ExitReason`] has no term variant, so such a raise is
/// [`ExitReason::Structured`] and the reason *term* itself (`{badkey, K}`,
/// `{badmap, M}`, `{case_clause, V}`, …) is the payload of
/// [`Raise::live_reason`](beam_common::Raise::live_reason) — the
/// `beam_bifs`/`beam_engine` convention, whose payload type is
/// [`Eterm`]. This is the **single** place that recovers it, so every consumer
/// (`crate::runner`'s `exit_reason`, the `run` diagnostic in `crate::cli`)
/// downcasts the same way instead of repeating the incantation.
///
/// Returns `None` for anything that is not such a raise, **and** for a raise whose
/// reason *is* an atom: an atom reason is the authoritative one (the oracle's
/// `undef`, `badarg`, `unsupported_opcode: …`, …), so a term attached next to one
/// must never shadow it. The gate reads [`ExitReason::as_atom`], which — unlike
/// [`BeamError::as_atom`] — also sees atoms backed by an owned `String` (every
/// message this module builds).
pub(crate) fn live_reason_term(error: &BeamError) -> Option<Eterm> {
    let BeamError::Raise(raise) = error else {
        return None;
    };
    if raise.reason.as_atom().is_some() {
        return None;
    }
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
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

/// The `erlang`-namespace calls classified as **known** capability gaps rather than the
/// *plain* `erlang:X/Y is not a BIF` diagnostic a **genuinely unknown** `erlang` name
/// gets (such a name raises the bare [`undef`] the oracle does): that would present a
/// documented capability gap as a genuine runtime error. A listed call is classified as
/// *unsupported* instead (exit code [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED)) —
/// see [`known_unimplemented_bif`].
///
/// # Membership: exactly one rule
///
/// A `(function, arity)` belongs here **iff** the OTP release really **exports** the name
/// from the `erlang` module **and** this runtime does not serve it. Nothing else qualifies:
/// membership means *"an OTP `erlang` BIF the oracle exports but this runtime does not
/// serve"* — a capability gap this runtime can honestly report as such.
///
/// A name the **oracle itself** rejects as `undef` because `erlang` does not export it
/// (`erlang:apply/1`, `erlang:function_exported/2`, `erlang:load_module/3`,
/// `erlang:system_info/2`, `erlang:statistics/0`, `erlang:map_get/3`,
/// `erlang:concatenate/2`, `erlang:register/3`, `erlang:system_profile/1` — all verified
/// with `erlang:function_exported(erlang, F, A)` and a direct call on OTP 29.1.1 /
/// erts-17.1, both `false`/`error:undef`) therefore must **not** be listed: curating one
/// would answer the oracle's exit-1 "no such function" with an exit-3 capability gap, a
/// divergence dressed up as a gap. Such a name falls through to [`undef`], the honest
/// reproduction of the oracle's *absence*.
///
/// This list is the **candidate** set, not the answer: [`is_known_unimplemented_bif`]
/// intersects it with the live `beam_bifs` registry at use time, so a name the registry
/// has since grown is never shadowed and the two can never drift apart (see the
/// [`is_known_unimplemented_bif`] docs). An entry the *registry* owns therefore violates
/// this rule and is inert — the intersection enforces the rule, so such an entry can never
/// change an answer. An entry only the runtime-native floor (`crate::native`, which runs
/// *before* the registry and this fallback — `set_cookie/2` today) serves never reaches the
/// fallback either: inert too, and equally to be removed when noticed.
///
/// `beam_bifs` now registers **every** declared `bif.tab` entry, routing each through its
/// `RuntimeHooks` seam (the typed `unsupported_bif` capability gap until the runtime
/// installs a hook). A name the registry owns is therefore reached by `call_bif` *before*
/// this fallback and must never be listed here; what remains is the residue of
/// `erlang`-namespace calls that carry **no** declared `bif.tab` entry under the `erlang`
/// module — the OTP `erlang` functions whose implementation is an `erts_internal:*` BIF
/// (`port_control/3`, `process_flag/3`, `suspend_process/1,2`, `system_monitor/1`, ...) or an
/// `erlang.erl` wrapper, which the registry cannot own.
/// The list is curated rather than generated: a name is added once the oracle is confirmed
/// to export it *and* it is noticed reaching the fallback with the plain `not a BIF`
/// diagnostic.
pub(crate) const KNOWN_UNIMPLEMENTED_ERLANG_BIFS: &[(&str, u32)] = &[
    // The distribution/node surface the release carries without an `erlang`-module
    // `bif.tab` entry.
    ("set_cookie", 2),
    ("disconnect_node", 1),
    // Ports: declared as `erts_internal:port_*`.
    ("port_connect", 2),
    ("port_call", 3),
    // Spawning on a remote node: the whole `spawn_request` family is exported by the
    // oracle, and no arity of it carries an `erlang`-module `bif.tab` entry (the
    // `erts_internal:spawn_request/4` the registry does own is a different key).
    ("spawn_request", 1),
    ("spawn_request", 2),
    ("spawn_request", 3),
    ("spawn_request", 4),
    ("spawn_request", 5),
    // Process control: `process_flag/3`, `suspend_process/1,2` and `process_display/2` are
    // declared as `erts_internal:` BIFs.
    ("process_flag", 3),
    ("process_display", 2),
    ("suspend_process", 1),
    ("suspend_process", 2),
    ("system_monitor", 0),
    ("system_monitor", 1),
    ("system_monitor", 2),
    // Garbage collection: `erts_internal:garbage_collect/1` hosts the collector entry, and
    // `/0`+`/1`+`/2` are all served by the runtime-native floor
    // (`src/native/erts_internal.rs`), so no arity of it is a gap.
    // Code loading.
    ("load_module", 2),
    // The system/time surface.
    ("localtime_to_universaltime", 1),
];

/// A **known** `erlang` BIF that Milestone-1 deliberately does not implement.
///
/// Reporting it as the bare [`undef`] a typo (`erlang:nope/1`) gets would be
/// indistinguishable from that typo and, worse, would classify a documented capability gap
/// as a genuine runtime error. The reason atom therefore starts with `unsupported`, so
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

/// The raise a NIF recorded on its environment (`enif_raise_exception`, `enif_make_badarg`).
///
/// The reason is a **term** the C NIF built, so it travels in the live-reason slot
/// (`ExitReason::Structured` + [`LiveReason`]) — the same carrier the engine and
/// `beam_bifs` use for a structured raise reason. [`live_reason_term`] recovers it and the
/// single printer renders it, so `enif_make_badarg` surfaces as the oracle's `error:badarg`
/// and a NIF's own reason term is never replaced by a placeholder.
pub(crate) fn nif_raised(reason: Eterm) -> BeamError {
    BeamError::Raise(
        Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(reason)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::{ExceptionClass, LiveReason};
    use beam_term::{make_atom, make_small};

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
    fn a_term_heap_exhaustion_becomes_the_documented_system_limit() {
        use beam_common::AllocFailure;
        // Both exhaustion classes mean *collect and retry*, which Milestone 1 cannot do: the
        // per-process heap bump allocator (`AllocKind::Heap`) and the copy engine's
        // destination-heap exhaustion (`AllocKind::Message` — the message heap). The honest
        // answer is the emulator's `error:system_limit`, never the internal
        // `allocation failure: heap|message (N bytes)` text.
        for kind in [AllocKind::Heap, AllocKind::Message] {
            let exhausted = BeamError::Alloc(AllocFailure::new(kind, 800));
            assert!(
                matches!(enforce_term_heap_limit(exhausted), BeamError::SystemLimit),
                "AllocKind::{kind:?} exhaustion must translate to system_limit"
            );
        }
    }

    #[test]
    fn the_translation_passes_every_other_error_through_unchanged() {
        use beam_common::AllocFailure;
        // The translation must swallow nothing and must not over-generalize: a **different**
        // allocation class (a carrier/large-object/other host OOM is not the emulator's
        // documented `system_limit`) and a non-allocation error both survive verbatim.
        for error in [
            BeamError::Alloc(AllocFailure::new(AllocKind::Carrier, 800)),
            BeamError::Alloc(AllocFailure::new(AllocKind::LargeObject, 800)),
            BeamError::Alloc(AllocFailure::new(AllocKind::Other, 800)),
            BeamError::TermNotFound,
            BeamError::BadArg,
            undef(),
        ] {
            let rendered = error.to_string();
            let translated = enforce_term_heap_limit(error);
            assert!(
                !matches!(translated, BeamError::SystemLimit),
                "only a Heap/Message exhaustion is translated"
            );
            assert_eq!(translated.to_string(), rendered, "the error is unchanged");
        }
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
    fn an_abort_round_trips_and_matches_nothing_else() {
        let abort = abort();
        assert!(is_abort(&abort));
        // An abort is terminal but is neither a halt status nor a capability gap …
        assert_eq!(halt_status(&abort), None);
        assert_eq!(unsupported_reason(&abort), None);
        assert_eq!(dump_slogan(&abort), None);
        // … and only the bare `beam_rt_abort` atom is an abort.
        assert!(!is_abort(&halt(1)));
        assert!(!is_abort(&dump("slogan")));
        assert!(!is_abort(&BeamError::BadArg));
        assert!(!is_abort(&BeamError::SystemLimit));
        assert!(!is_abort(&message("boom")));
        let padded = BeamError::Raise(Raise::exit(ExitReason::atom(String::from(
            "beam_rt_abort_now",
        ))));
        assert!(!is_abort(&padded));
    }

    #[test]
    fn a_dump_slogan_round_trips_and_matches_nothing_else() {
        assert_eq!(dump_slogan(&dump("Probe 1")), Some("Probe 1"));
        // The empty slogan (`halt([])`) survives the round trip too.
        assert_eq!(dump_slogan(&dump("")), Some(""));
        // A slogan is neither a halt status, an abort nor a capability gap.
        assert_eq!(halt_status(&dump("s")), None);
        assert!(!is_abort(&dump("s")));
        assert_eq!(unsupported_reason(&dump("s")), None);
        // Anything that is not a dump raise is `None` — never a substituted slogan.
        for other in [halt(1), abort(), BeamError::BadArg, message("boom")] {
            assert_eq!(dump_slogan(&other), None, "{other}");
        }
        // The convention is exact: a reason that merely *starts* with the prefix is not a
        // dump raise.
        let padded = BeamError::Raise(Raise::exit(ExitReason::atom(String::from(
            "beam_rt_dumps",
        ))));
        assert_eq!(dump_slogan(&padded), None);
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
    fn the_live_reason_term_is_recovered_only_for_a_structured_reason() {
        let term = beam_term::make_small(42);
        // A raise whose reason is a non-atom carries the reason *term* in the
        // live-reason slot (`ExitReason::Structured` + `LiveReason`).
        let structured = BeamError::Raise(
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(term)),
        );
        assert_eq!(live_reason_term(&structured), Some(term));
        // … and one that carries no term at all has none to recover.
        assert_eq!(
            live_reason_term(&BeamError::Raise(Raise::error(ExitReason::Structured))),
            None
        );
        // An **atom** reason is authoritative: a payload stored next to it must never
        // shadow it (including an owned-`String` atom, invisible to `BeamError::as_atom`).
        let atom = BeamError::Raise(
            Raise::error(ExitReason::atom(String::from("badmatch")))
                .with_live_reason(LiveReason::new(term)),
        );
        assert_eq!(live_reason_term(&atom), None);
        // A non-raise carries no live reason at all.
        assert_eq!(live_reason_term(&BeamError::BadArg), None);
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

    /// A raise carrying `term` as its (structured) live reason.
    fn structured_raise(class: ExceptionClass, term: Eterm) -> BeamError {
        BeamError::Raise(
            Raise::new(class, ExitReason::Structured).with_live_reason(LiveReason::new(term)),
        )
    }

    /// A seeded shared atom table (the boot seed is what makes a core name statically
    /// spellable and nameable).
    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        crate::boot::seed_core_atoms(&mut atoms);
        atoms
    }

    #[test]
    fn a_bare_atom_live_reason_is_recovered_as_an_atom_reason() {
        // The engine's uncaught raise carries its reason term-independently
        // (`ExitReason::Structured`) with the term in the live-reason slot; for the
        // *bare atom* `function_clause` the name is recoverable, so the term-independent
        // model must report it instead of the `<structured reason>` placeholder.
        let atoms = seeded_atoms();
        let term = make_atom(atoms.lookup("function_clause").expect("seeded"));
        let error = structured_raise(ExceptionClass::Error, term);

        let named = name_atom_raise(error, &atoms);
        assert!(named.is_raise());
        assert_eq!(named.as_atom(), Some("function_clause"));
        assert_eq!(named.to_string(), "error function_clause");
        assert_eq!(unsupported_reason(&named), None);
        // The recovery is not a substitution: the live reason term survives verbatim, so
        // landing in a catch frame still delivers the term itself.
        let raise = named.into_raise().expect("a raise");
        assert_eq!(raise.class, ExceptionClass::Error);
        assert_eq!(raise.reason.as_atom(), Some("function_clause"));
        assert_eq!(
            raise.live_reason().and_then(|live| live.downcast_ref::<Eterm>()),
            Some(&term)
        );
    }

    #[test]
    fn the_recovered_name_is_static_for_a_core_seed_atom() {
        // `BeamError::as_atom` hands back a `&'static str`, so a recovered name that the
        // runtime's own core seed spells is reported statically; a name only the live
        // table knows keeps the owned form (Display still names it).
        let atoms = seeded_atoms();
        for name in ["function_clause", "exit", "throw", "normal"] {
            let term = make_atom(atoms.lookup(name).expect("seeded"));
            let named = name_atom_raise(structured_raise(ExceptionClass::Error, term), &atoms);
            assert_eq!(named.as_atom(), Some(name), "{name}");
        }
        let local = make_atom(atoms.lookup("preloaded").expect("seeded"));
        let named = name_atom_raise(structured_raise(ExceptionClass::Error, local), &atoms);
        assert_eq!(named.as_atom(), Some("preloaded"));
        // A name the seed does not carry and the table did not have: interned here, so it
        // is nameable — just not statically.
        let mut atoms = atoms;
        let fresh = make_atom(atoms.intern("not_a_core_atom"));
        let named = name_atom_raise(structured_raise(ExceptionClass::Error, fresh), &atoms);
        assert_eq!(named.as_atom(), None);
        assert_eq!(named.to_string(), "error not_a_core_atom");
        assert_eq!(unsupported_reason(&named), None);
    }

    #[test]
    fn a_non_atom_live_reason_keeps_the_structured_placeholder() {
        // A live term that is not an atom (here a small int, standing in for a compound
        // reason) is genuinely term-independent: the placeholder must stay
        // (`as_atom()` is `None`) and the live term must be untouched.
        let atoms = seeded_atoms();
        let reason = make_small(1);
        let named = name_atom_raise(structured_raise(ExceptionClass::Error, reason), &atoms);
        assert_eq!(named.as_atom(), None);
        assert_eq!(named.to_string(), "error <structured reason>");
        assert_eq!(
            named.into_raise().and_then(|raise| raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
                .copied()),
            Some(reason)
        );
    }
    #[test]
    fn raises_without_a_live_term_and_non_raises_are_unchanged() {
        let atoms = seeded_atoms();
        // A raise whose reason is already an atom: untouched.
        let atom_raise = BeamError::Raise(Raise::error(ExitReason::atom("badmatch")));
        let named = name_atom_raise(atom_raise, &atoms);
        assert_eq!(named.as_atom(), Some("badmatch"));
        // A structured raise with NO live term: untouched (nothing to recover from).
        let bare = structured_raise(ExceptionClass::Error, make_small(1));
        let bare = match bare {
            BeamError::Raise(raise) => BeamError::Raise(Raise::new(raise.class, raise.reason)),
            other => other,
        };
        assert_eq!(name_atom_raise(bare, &atoms).as_atom(), None);
        // A non-raise is returned as-is.
        assert!(matches!(
            name_atom_raise(BeamError::SystemLimit, &atoms),
            BeamError::SystemLimit
        ));
    }
    #[test]
    fn diagnostics_name_the_offending_item() {
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
        // The curated list is the **candidate** set of `erlang`-namespace calls this runtime
        // reaches with the plain `not a BIF` diagnostic. Derive the invariant from the live
        // registry rather than hardcoding names: a curated entry must never be a name the
        // registry already owns (the registry is consulted first, so such an entry is dead
        // weight and would mis-describe a served BIF as a capability gap).
        assert!(
            !KNOWN_UNIMPLEMENTED_ERLANG_BIFS.is_empty(),
            "the list carries the `erlang` gaps the registry does not own"
        );
        for &(function, arity) in KNOWN_UNIMPLEMENTED_ERLANG_BIFS {
            assert!(
                !registry.contains("erlang", function, arity),
                "erlang:{function}/{arity} is registry-owned and must not be curated"
            );
            assert!(
                is_known_unimplemented_bif(&registry, function, arity),
                "erlang:{function}/{arity} is curated and unregistered: an effective gap"
            );
            // A curated entry is a documented capability gap, classified by the same
            // convention as every other `unsupported*` raise (so the CLI exits 3), not the
            // plain `is not a BIF` diagnostic a typo gets.
            let reason = unsupported_reason(&known_unimplemented_bif(function, arity))
                .expect("a curated gap is a capability gap");
            assert!(
                reason.contains(&format!("erlang:{function}/{arity}")),
                "the gap diagnostic names erlang:{function}/{arity}: {reason:?}"
            );
        }
        // A name that is neither curated nor registered is not a known gap: it gets the bare
        // `undef` a typo gets.
        assert!(!listed_as_unimplemented("nope", 1));
        assert!(!is_known_unimplemented_bif(&registry, "nope", 1));
    }

    /// The two-way answer the `erlang`-namespace fallback gives once the native floor and the
    /// `beam_bifs` registry have both declined a call (`context/mod.rs::call_bif`): a curated
    /// name is the typed capability gap, anything else the bare `undef` the oracle raises.
    fn erlang_fallback(registry: &BifRegistry, function: &str, arity: u32) -> BeamError {
        if is_known_unimplemented_bif(registry, function, arity) {
            known_unimplemented_bif(function, arity)
        } else {
            undef()
        }
    }

    /// Membership means exactly *"an OTP `erlang` BIF the oracle **exports** but this runtime
    /// does not serve"* — so a name the OTP release does not export at all must never be
    /// curated.
    ///
    /// Each name below was curated by an earlier revision although `erlang` does **not** export
    /// it: `erlang:function_exported(erlang, F, A)` is `false` and a call raises
    /// `error:undef` (verified against the oracle — OTP 29.1.1 / erts-17.1 — with
    /// `erl -noshell -eval 'try erlang:F(A…) catch C:R -> {C,R} end, halt().'`). Curating one
    /// answered the oracle's exit-1 "no such function" with an exit-3 capability gap, a
    /// divergence presented as a gap. They must classify as **not** a gap, i.e. reach the bare
    /// [`undef`] — never come back.
    #[test]
    fn a_name_the_otp_release_does_not_export_is_not_a_curated_gap() {
        let registry = beam_bifs::default_registry();
        for (function, arity) in [
            ("apply", 1u32),
            ("function_exported", 2),
            ("load_module", 3),
            ("system_info", 2),
            ("statistics", 0),
            ("map_get", 3),
            ("concatenate", 2),
            ("register", 3),
            ("system_profile", 1),
        ] {
            assert!(
                !listed_as_unimplemented(function, arity),
                "erlang:{function}/{arity} is not exported by the release and must not be curated"
            );
            let answer = erlang_fallback(&registry, function, arity);
            assert_eq!(
                unsupported_reason(&answer),
                None,
                "erlang:{function}/{arity} is the oracle's exit-1 `undef`, never a capability gap"
            );
            assert_eq!(
                answer.as_atom(),
                Some("undef"),
                "erlang:{function}/{arity} answers the bare reason atom the oracle raises"
            );
        }
    }

    /// The **exit codes** the differential harness keys off, for both directions of the rule:
    /// a reclassified name is the oracle's exit-1 `error:undef`, a newly curated one the exit-3
    /// capability gap — the very signal [`unsupported_reason`] is the single interpreter of.
    #[test]
    fn a_reclassified_name_exits_1_and_a_newly_curated_one_exits_3() {
        // `erlang` does not export `statistics/0`, so the oracle exits 1 with `error:undef`;
        // an earlier revision answered exit 3 here.
        assert_eq!(
            crate::cli::run(vec!["-eval".to_string(), "erlang:statistics()".to_string()]),
            crate::cli::EXIT_RUNTIME,
            "a name the release does not export is the oracle's genuine `undef`, exit 1"
        );
        // The reverse direction: these two **are** exported by the oracle and are not served
        // here, so the honest answer is the capability gap, not a bare `undef`.
        for expr in [
            "erlang:spawn_request(self())",
            "erlang:suspend_process(self(), true)",
        ] {
            assert_eq!(
                crate::cli::run(vec!["-eval".to_string(), expr.to_string()]),
                crate::cli::EXIT_UNSUPPORTED,
                "{expr} is an exported-but-unserved erlang BIF: a capability gap, exit 3"
            );
        }
    }

    #[test]
    fn the_spawn_family_is_no_longer_a_curated_gap() {
        // The native floor serves `spawn/1,2,3`, `spawn_link/1,2,3` and `spawn_opt/2,3,4`.
        // Keeping any of them in the curated list would be dead weight (they are answered
        // before the `erlang`-namespace fallback is reached at all) and the `/4` arities
        // OTP does not have must not be presented as a documented capability gap either.
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
        ] {
            assert!(
                !listed_as_unimplemented(function, arity),
                "erlang:{function}/{arity} must no longer be a curated gap"
            );
        }
        // `port_command/2,3` is no longer curated either: the native floor serves it from
        // `src/native/port.rs` (writing to the port child's stdin), so keeping it on the
        // list would be dead weight — and answering it as a documented capability gap
        // would be wrong.
        for (function, arity) in [("port_command", 2u32), ("port_command", 3)] {
            assert!(
                !listed_as_unimplemented(function, arity),
                "erlang:{function}/{arity} is served by the native floor and must not be a curated gap"
            );
        }
    }

    #[test]
    fn the_effective_gap_set_never_shadows_the_registry() {
        // The **effective** gap set — the curated list intersected with what the live
        // registry does *not* own — must be disjoint from the registry. That is the drift
        // guard: a `beam_bifs` change that registers a name the static list still carries can
        // never turn a served BIF into an `unsupported` capability gap, because the
        // intersection drops it. Derive the check from the live registry rather than
        // hardcoding names.
        let registry = beam_bifs::default_registry();
        let mut registered_erlang = 0usize;
        for (module, function, arity) in registry.names() {
            if module != "erlang" {
                continue;
            }
            registered_erlang += 1;
            assert!(
                !is_known_unimplemented_bif(&registry, &function, arity),
                "erlang:{function}/{arity} is a registered BIF and must not be classified as an unimplemented gap"
            );
        }
        // The guard is not vacuous: the registry really does own `erlang` keys (so the loop
        // above exercises the drift this intersection absorbs) …
        assert!(
            registered_erlang > 0,
            "the registry owns `erlang` BIFs for the drift guard to exercise"
        );
        // … and a curated name the registry does *not* own still classifies as a gap.
        let (function, arity) = KNOWN_UNIMPLEMENTED_ERLANG_BIFS
            .iter()
            .find(|&&(function, arity)| !registry.contains("erlang", function, arity))
            .copied()
            .expect("a curated name the registry does not own");
        assert!(!registry.contains("erlang", function, arity));
        assert!(is_known_unimplemented_bif(&registry, function, arity));
    }
}
