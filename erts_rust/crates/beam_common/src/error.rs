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

//! The workspace error type and the term-independent BEAM exception machinery.
//!
//! This module owns the workspace-wide error strategy (Part I §A.1 / Part II §B):
//! BIF and NIF failure is **return-based** — there is no `setjmp`/`longjmp`, no
//! panic-as-control-flow and no silent swallowing. Fallible operations return a
//! [`Result<T>`]; the execution engine turns a [`BeamError`] (in particular
//! [`BeamError::Raise`]) into the actual BEAM raise by placing the exception tag
//! in `x(0)`.
//!
//! Everything in this module is **term-independent** — `beam_common` must not
//! depend on `beam_term`. A structured (tuple/list/map) reason is therefore
//! represented here by [`ExitReason::Structured`], and its live term is carried
//! out-of-band across the boundary in the opaque [`Raise::live_reason`] slot.

use std::any::Any;
use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

/// The workspace result type: [`BeamError`] as the single error type.
///
/// Every fallible operation in the Rust ERTS workspace returns this alias so that
/// error propagation is uniform and return-based (never `setjmp`/`longjmp`).
pub type Result<T> = core::result::Result<T, BeamError>;

/// The one and only error type of the Rust ERTS workspace.
///
/// Reason variants ([`BadArg`](BeamError::BadArg), [`BadArith`](BeamError::BadArith),
/// [`BadArity`](BeamError::BadArity), [`BadMatch`](BeamError::BadMatch),
/// [`SystemLimit`](BeamError::SystemLimit), [`Alloc`](BeamError::Alloc)) map onto the
/// corresponding BEAM error atoms (or `out_of_memory` for allocation failure);
/// [`Raise`](BeamError::Raise) carries an already-built BEAM exception, and the
/// remaining variants describe runtime plumbing (lookup failures, I/O).
#[derive(Debug)]
#[non_exhaustive]
pub enum BeamError {
    /// `badarg`: an argument failed a type/range/format check (BEAM error atom `badarg`).
    BadArg,
    /// `badarith`: arithmetic on a non-numeric term (BEAM error atom `badarith`).
    BadArith,
    /// `badarity`: calling a fun or BIF with the wrong number of arguments
    /// (BEAM error atom `badarity`).
    BadArity,
    /// `badmatch`: a pattern match failed (BEAM error atom `badmatch`).
    BadMatch,
    /// `system_limit`: a hard runtime limit was exceeded (BEAM error atom `system_limit`).
    SystemLimit,
    /// An allocation failed (reported as the `out_of_memory` atom); carries the
    /// requested size and the allocation class.
    Alloc(AllocFailure),
    /// An internal term/table/registry lookup found nothing. This is runtime
    /// plumbing and has no BEAM error atom (`as_atom` returns `None`).
    TermNotFound,
    /// An operating-system I/O error (no BEAM error atom; use
    /// [`source`](Error::source) to reach the underlying [`io::Error`]).
    Io(std::io::Error),
    /// An explicit BEAM exception to be raised by the engine.
    Raise(Raise),
}

impl BeamError {
    /// Returns the BEAM error atom name this error raises, when there is one.
    ///
    /// * [`BadArg`](BeamError::BadArg) → `"badarg"`, [`BadArith`](BeamError::BadArith) →
    ///   `"badarith"`, [`BadArity`](BeamError::BadArity) → `"badarity"`,
    ///   [`BadMatch`](BeamError::BadMatch) → `"badmatch"`,
    ///   [`SystemLimit`](BeamError::SystemLimit) → `"system_limit"`,
    ///   [`Alloc`](BeamError::Alloc) → `"out_of_memory"`.
    /// * [`Raise`](BeamError::Raise) → the atom name of its reason, but only when that
    ///   reason *is* an atom ([`ExitReason::atom`]). A `Raise` whose reason is
    ///   [`ExitReason::Structured`], or an [`AtomName`] backed by an owned `String`
    ///   (which cannot be returned as `&'static str`), yields `None`.
    /// * [`TermNotFound`](BeamError::TermNotFound) and [`Io`](BeamError::Io) → `None`.
    pub fn as_atom(&self) -> Option<&'static str> {
        match self {
            BeamError::BadArg => Some("badarg"),
            BeamError::BadArith => Some("badarith"),
            BeamError::BadArity => Some("badarity"),
            BeamError::BadMatch => Some("badmatch"),
            BeamError::SystemLimit => Some("system_limit"),
            BeamError::Alloc(_) => Some("out_of_memory"),
            BeamError::TermNotFound | BeamError::Io(_) => None,
            BeamError::Raise(raise) => raise.reason.static_atom(),
        }
    }

    /// Returns `true` when this error carries an explicit BEAM exception.
    ///
    /// Only [`BeamError::Raise`] is a raise; the reason variants are converted into a
    /// raise by the engine.
    pub fn is_raise(&self) -> bool {
        matches!(self, BeamError::Raise(_))
    }

    /// Consumes the error and returns the carried [`Raise`], if any.
    pub fn into_raise(self) -> Option<Raise> {
        match self {
            BeamError::Raise(raise) => Some(raise),
            _ => None,
        }
    }
}

impl fmt::Display for BeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BeamError::BadArg => f.write_str("badarg"),
            BeamError::BadArith => f.write_str("badarith"),
            BeamError::BadArity => f.write_str("badarity"),
            BeamError::BadMatch => f.write_str("badmatch"),
            BeamError::SystemLimit => f.write_str("system_limit"),
            BeamError::Alloc(failure) => write!(f, "{failure}"),
            BeamError::TermNotFound => f.write_str("term not found"),
            BeamError::Io(error) => write!(f, "io error: {error}"),
            BeamError::Raise(raise) => write!(f, "{raise}"),
        }
    }
}

impl Error for BeamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            BeamError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for BeamError {
    fn from(error: io::Error) -> Self {
        BeamError::Io(error)
    }
}

/// The details of a failed allocation.
///
/// Reported as the BEAM error atom `out_of_memory`; the class and the requested
/// byte count are retained for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AllocFailure {
    /// The number of bytes that were requested.
    pub requested: usize,
    /// The kind of allocation that failed.
    pub kind: AllocKind,
}

impl AllocFailure {
    /// Creates an allocation failure for `kind` requesting `requested` bytes.
    pub fn new(kind: AllocKind, requested: usize) -> Self {
        Self { requested, kind }
    }
}

impl fmt::Display for AllocFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "allocation failure: {} ({} bytes)",
            self.kind.as_str(),
            self.requested
        )
    }
}

/// The class of an allocation that failed, matching the `erts_alloc` families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AllocKind {
    /// A process or shared heap.
    Heap,
    /// An off-heap large object (MSO).
    LargeObject,
    /// An allocator carrier (a slab of heap memory handed to many allocations).
    Carrier,
    /// A message fragment or message buffer.
    Message,
    /// Any other allocation.
    Other,
}

impl AllocKind {
    /// The stable, snake_case name of this allocation class.
    ///
    /// Returns `"heap"`, `"large_object"`, `"carrier"`, `"message"` or `"other"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            AllocKind::Heap => "heap",
            AllocKind::LargeObject => "large_object",
            AllocKind::Carrier => "carrier",
            AllocKind::Message => "message",
            AllocKind::Other => "other",
        }
    }
}

/// The class of a BEAM exception, as reported by `catch` and the error logger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExceptionClass {
    /// An `erlang:error/1,2` exception.
    Error,
    /// An `exit/1` (or process exit) exception.
    Exit,
    /// A `throw/1` exception.
    Throw,
}

impl ExceptionClass {
    /// The lowercase name of this class: `"error"`, `"exit"` or `"throw"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExceptionClass::Error => "error",
            ExceptionClass::Exit => "exit",
            ExceptionClass::Throw => "throw",
        }
    }
}

/// The reason of a BEAM exception or process exit, term-independently.
///
/// The well-known atoms of the runtime are modelled as dedicated variants
/// (compare against [`as_atom`](ExitReason::as_atom)); any other atom is carried as
/// [`Atom`](ExitReason::Atom). Reasons that are not atoms at all (tuples, lists,
/// maps, …) are represented by [`Structured`](ExitReason::Structured): this enum
/// holds no term, but the live reason term is carried alongside it in the
/// [`Raise::live_reason`] slot by the raising crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitReason {
    /// The `normal` exit reason.
    Normal,
    /// The `shutdown` exit reason.
    Shutdown,
    /// The `noproc` exit reason (linking/calling a non-existent process).
    NoProc,
    /// The `noconnection` exit reason (distribution link/send to a lost node).
    NoConnection,
    /// The `killed` exit reason (an untrappable kill).
    Killed,
    /// The `aborted` exit reason (runtime-forced termination).
    Aborted,
    /// Any other atom reason.
    Atom(AtomName),
    /// A non-atom (structured) reason; the live term is carried alongside it in
    /// [`Raise::live_reason`], because `beam_common` has no term dependency.
    Structured,
}

impl ExitReason {
    /// Builds [`ExitReason::Atom`] from anything convertible into an [`AtomName`].
    pub fn atom(name: impl Into<AtomName>) -> Self {
        ExitReason::Atom(name.into())
    }

    /// The atom name of this reason, when it is an atom.
    ///
    /// Returns `Some` for the well-known variants and for [`Atom`](ExitReason::Atom);
    /// returns `None` for [`Structured`](ExitReason::Structured).
    pub fn as_atom(&self) -> Option<&str> {
        match self {
            ExitReason::Normal => Some("normal"),
            ExitReason::Shutdown => Some("shutdown"),
            ExitReason::NoProc => Some("noproc"),
            ExitReason::NoConnection => Some("noconnection"),
            ExitReason::Killed => Some("killed"),
            ExitReason::Aborted => Some("aborted"),
            ExitReason::Atom(name) => Some(name.as_str()),
            ExitReason::Structured => None,
        }
    }

    /// Like [`as_atom`](ExitReason::as_atom), but only for names that are backed by
    /// a `&'static str` (used by [`BeamError::as_atom`], whose result is `&'static str`).
    fn static_atom(&self) -> Option<&'static str> {
        match self {
            ExitReason::Normal => Some("normal"),
            ExitReason::Shutdown => Some("shutdown"),
            ExitReason::NoProc => Some("noproc"),
            ExitReason::NoConnection => Some("noconnection"),
            ExitReason::Killed => Some("killed"),
            ExitReason::Aborted => Some("aborted"),
            ExitReason::Atom(name) => name.static_str(),
            ExitReason::Structured => None,
        }
    }
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitReason::Normal => f.write_str("normal"),
            ExitReason::Shutdown => f.write_str("shutdown"),
            ExitReason::NoProc => f.write_str("noproc"),
            ExitReason::NoConnection => f.write_str("noconnection"),
            ExitReason::Killed => f.write_str("killed"),
            ExitReason::Aborted => f.write_str("aborted"),
            ExitReason::Atom(name) => write!(f, "{name}"),
            ExitReason::Structured => f.write_str("<structured reason>"),
        }
    }
}

/// An opaque handle to the **live reason term** of a BEAM exception.
///
/// `beam_common` is a dependency sink and must not depend on `beam_term`, so the
/// term-independent [`ExitReason`] cannot itself hold a structured reason
/// ([`ExitReason::Structured`] is a unit variant). The raising crate therefore
/// stores its live reason *term* in this handle and attaches it to the [`Raise`]
/// with [`with_live_reason`](Raise::with_live_reason); consumers recover the term
/// with [`downcast_ref`](LiveReason::downcast_ref).
///
/// # Convention
///
/// The payload is the raising crate's live reason term: a crate that has a term
/// representation stores its reason term there, and a consumer recovers it with
/// `downcast_ref::<Eterm>()`. `beam_common` deliberately names no term type — the
/// `Eterm` above is the payload type chosen by the adopter, not a type this crate
/// depends on.
///
/// The payload is **out-of-band metadata**: it is ignored by [`Raise`]'s
/// `PartialEq`/`Eq` and is never inspected by its `Debug` implementation.
///
/// The handle is `Clone` + `Send` + `Sync` (the payload lives behind an [`Arc`]),
/// so a raise carrying it can be propagated and shared across threads.
#[derive(Clone)]
pub struct LiveReason(Arc<dyn Any + Send + Sync>);

impl LiveReason {
    /// Wraps `value` as an opaque live reason payload.
    pub fn new<T: Any + Send + Sync + 'static>(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// Borrows the payload as `T`, or `None` when the payload holds a different
    /// type.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }

    /// Borrows the payload as a `&dyn Any` for further inspection.
    pub fn as_any(&self) -> &dyn Any {
        self.0.as_ref()
    }
}

impl fmt::Debug for LiveReason {
    /// Renders the placeholder `<live reason>`; the payload is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<live reason>")
    }
}

/// A complete BEAM exception: class, reason, stacktrace and — for a structured
/// reason — the live reason term.
///
/// Equality and `Debug` cover only the observable parts ([`class`](Raise::class),
/// [`reason`](Raise::reason), [`stacktrace`](Raise::stacktrace)): the
/// [`live_reason`](Raise::live_reason) payload is out-of-band metadata that
/// [`ExitReason`] cannot compare, so a raise carrying a live term compares equal to
/// the same raise without it.
#[derive(Clone)]
pub struct Raise {
    /// The class of the exception.
    pub class: ExceptionClass,
    /// The reason of the exception.
    pub reason: ExitReason,
    /// The stacktrace captured at the raise point (empty if unavailable).
    pub stacktrace: StackTrace,
    /// The live reason term, when the reason is structured (see [`LiveReason`]).
    pub live_reason: Option<LiveReason>,
}

impl Raise {
    /// Creates a raise of `class` with `reason`, an empty stacktrace and no live
    /// reason.
    pub fn new(class: ExceptionClass, reason: ExitReason) -> Self {
        Self {
            class,
            reason,
            stacktrace: StackTrace::new(),
            live_reason: None,
        }
    }

    /// Creates an [`ExceptionClass::Error`] raise.
    pub fn error(reason: ExitReason) -> Self {
        Self::new(ExceptionClass::Error, reason)
    }

    /// Creates an [`ExceptionClass::Exit`] raise.
    pub fn exit(reason: ExitReason) -> Self {
        Self::new(ExceptionClass::Exit, reason)
    }

    /// Creates an [`ExceptionClass::Throw`] raise.
    pub fn throw(reason: ExitReason) -> Self {
        Self::new(ExceptionClass::Throw, reason)
    }

    /// Replaces the stacktrace, returning the updated raise (builder style).
    pub fn with_stacktrace(mut self, stacktrace: StackTrace) -> Self {
        self.stacktrace = stacktrace;
        self
    }

    /// Attaches the live reason term, returning the updated raise (builder style).
    pub fn with_live_reason(mut self, live_reason: LiveReason) -> Self {
        self.live_reason = Some(live_reason);
        self
    }

    /// The live reason term, when the raise carries one.
    pub fn live_reason(&self) -> Option<&LiveReason> {
        self.live_reason.as_ref()
    }
}

impl fmt::Debug for Raise {
    /// Prints the observable parts and the placeholder `<live reason>` for the
    /// opaque slot; the payload is never dereferenced or printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Raise")
            .field("class", &self.class)
            .field("reason", &self.reason)
            .field("stacktrace", &self.stacktrace)
            .field(
                "live_reason",
                &self.live_reason.as_ref().map(|_| "<live reason>"),
            )
            .finish()
    }
}

impl PartialEq for Raise {
    /// Compares only the observable parts; the opaque [`live_reason`](Raise::live_reason)
    /// payload is ignored.
    fn eq(&self, other: &Self) -> bool {
        self.class == other.class
            && self.reason == other.reason
            && self.stacktrace == other.stacktrace
    }
}

impl Eq for Raise {}

impl fmt::Display for Raise {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.class.as_str(), self.reason)
    }
}

/// An ordered BEAM stacktrace: the list of frames from the raise point outwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackTrace {
    /// The frames, outermost (raise point) first.
    pub frames: Vec<StackFrame>,
}

impl StackTrace {
    /// Creates an empty stacktrace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a frame (the frames are kept in the order they are pushed).
    pub fn push(&mut self, frame: StackFrame) {
        self.frames.push(frame);
    }

    /// Returns `true` when there are no frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The number of frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

impl fmt::Display for StackTrace {
    /// Renders one frame per line, in order; an empty stacktrace renders as the
    /// empty string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, frame) in self.frames.iter().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{frame}")?;
        }
        Ok(())
    }
}

/// One stack frame: a `module:function/arity` plus an optional source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// The module the frame belongs to.
    pub module: AtomName,
    /// The function the frame belongs to.
    pub function: AtomName,
    /// The arity of the function.
    pub arity: u8,
    /// The source location of the call, when known.
    pub location: Option<SourceLocation>,
}

impl StackFrame {
    /// Creates a frame with no source location.
    pub fn new(module: impl Into<AtomName>, function: impl Into<AtomName>, arity: u8) -> Self {
        Self {
            module: module.into(),
            function: function.into(),
            arity,
            location: None,
        }
    }
}

impl fmt::Display for StackFrame {
    /// Renders `module:function/arity`, followed by ` (file:line)` when the frame
    /// carries a source location.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.module, self.function, self.arity)?;
        if let Some(location) = &self.location {
            write!(f, " ({})", location)?;
        }
        Ok(())
    }
}

/// A source position inside a module, as produced by the compiler's line table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// The source file (usually a module-relative path).
    pub file: AtomName,
    /// The 1-based line number.
    pub line: u32,
}

impl fmt::Display for SourceLocation {
    /// Renders `file:line`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// A term-independent atom name.
///
/// This is deliberately **not** the interned atom of the term representation: it is
/// a plain owned-or-static string so that `beam_common` stays free of any
/// dependency on `beam_term`. The raising crate maps it onto the live atom table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AtomName(Cow<'static, str>);

impl AtomName {
    /// Creates an atom name from a `&'static str`, `String` or
    /// `Cow<'static, str>`.
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }

    /// The atom name as a string slice.
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    /// Returns the name as a `&'static str` when it is statically backed.
    fn static_str(&self) -> Option<&'static str> {
        match self.0 {
            Cow::Borrowed(name) => Some(name),
            Cow::Owned(_) => None,
        }
    }
}

impl fmt::Display for AtomName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for AtomName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&'static str> for AtomName {
    fn from(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }
}

impl From<String> for AtomName {
    fn from(name: String) -> Self {
        Self(Cow::Owned(name))
    }
}

impl From<Cow<'static, str>> for AtomName {
    fn from(name: Cow<'static, str>) -> Self {
        Self(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_error() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "boom")
    }

    fn sample_stacktrace() -> StackTrace {
        let mut stacktrace = StackTrace::new();
        stacktrace.push(StackFrame::new("lists", "map", 2));
        stacktrace.push(StackFrame {
            module: AtomName::new("erlang"),
            function: "apply".into(),
            arity: 3,
            location: Some(SourceLocation {
                file: AtomName::new("lists.erl"),
                line: 42,
            }),
        });
        stacktrace
    }

    #[test]
    fn display_covers_every_variant() {
        assert_eq!(BeamError::BadArg.to_string(), "badarg");
        assert_eq!(BeamError::BadArith.to_string(), "badarith");
        assert_eq!(BeamError::BadArity.to_string(), "badarity");
        assert_eq!(BeamError::BadMatch.to_string(), "badmatch");
        assert_eq!(BeamError::SystemLimit.to_string(), "system_limit");
        assert_eq!(
            BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 128)).to_string(),
            "allocation failure: heap (128 bytes)"
        );
        assert_eq!(BeamError::TermNotFound.to_string(), "term not found");
        assert_eq!(BeamError::Io(io_error()).to_string(), "io error: boom");
        assert_eq!(
            BeamError::Raise(Raise::error(ExitReason::atom("badarg"))).to_string(),
            "error badarg"
        );
    }

    #[test]
    fn alloc_kind_names_are_stable() {
        assert_eq!(AllocKind::Heap.as_str(), "heap");
        assert_eq!(AllocKind::LargeObject.as_str(), "large_object");
        assert_eq!(AllocKind::Carrier.as_str(), "carrier");
        assert_eq!(AllocKind::Message.as_str(), "message");
        assert_eq!(AllocKind::Other.as_str(), "other");
        assert_eq!(
            AllocFailure::new(AllocKind::LargeObject, 4096).requested,
            4096
        );
        assert_eq!(
            AllocFailure::new(AllocKind::Carrier, 8).kind,
            AllocKind::Carrier
        );
    }

    #[test]
    fn io_error_converts_and_is_the_source() {
        let error: BeamError = io_error().into();
        assert!(matches!(error, BeamError::Io(_)));
        assert!(error.source().is_some());
        assert_eq!(
            error.source().map(|source| source.to_string()),
            Some("boom".to_string())
        );
        assert!(BeamError::BadArg.source().is_none());
    }

    #[test]
    fn as_atom_maps_reason_variants() {
        assert_eq!(BeamError::BadArg.as_atom(), Some("badarg"));
        assert_eq!(BeamError::BadArith.as_atom(), Some("badarith"));
        assert_eq!(BeamError::BadArity.as_atom(), Some("badarity"));
        assert_eq!(BeamError::BadMatch.as_atom(), Some("badmatch"));
        assert_eq!(BeamError::SystemLimit.as_atom(), Some("system_limit"));
        assert_eq!(
            BeamError::Alloc(AllocFailure::new(AllocKind::Message, 16)).as_atom(),
            Some("out_of_memory")
        );
        assert_eq!(BeamError::TermNotFound.as_atom(), None);
        assert_eq!(BeamError::Io(io_error()).as_atom(), None);
    }

    #[test]
    fn as_atom_maps_raises_by_reason() {
        assert_eq!(
            BeamError::Raise(Raise::error(ExitReason::atom("badarg"))).as_atom(),
            Some("badarg")
        );
        assert_eq!(
            BeamError::Raise(Raise::exit(ExitReason::Normal)).as_atom(),
            Some("normal")
        );
        assert_eq!(
            BeamError::Raise(Raise::throw(ExitReason::Structured)).as_atom(),
            None
        );
        // Owned atom names cannot be handed out as `&'static str`.
        assert_eq!(
            BeamError::Raise(Raise::error(ExitReason::atom(String::from("custom")))).as_atom(),
            None
        );
    }

    #[test]
    fn raise_predicates_and_conversion() {
        let error = BeamError::Raise(Raise::throw(ExitReason::atom("boom")));
        assert!(error.is_raise());
        let raise = error.into_raise().expect("raise is carried");
        assert_eq!(raise.class, ExceptionClass::Throw);
        assert_eq!(raise.reason, ExitReason::atom("boom"));

        assert!(!BeamError::BadArg.is_raise());
        assert!(BeamError::BadArg.into_raise().is_none());
    }

    #[test]
    fn raise_builders_and_stacktrace() {
        let empty = Raise::new(ExceptionClass::Error, ExitReason::Normal);
        assert_eq!(empty.class, ExceptionClass::Error);
        assert!(empty.stacktrace.is_empty());

        assert_eq!(
            Raise::error(ExitReason::atom("badarg")).class,
            ExceptionClass::Error
        );
        assert_eq!(Raise::exit(ExitReason::Normal).class, ExceptionClass::Exit);
        assert_eq!(
            Raise::throw(ExitReason::Killed).class,
            ExceptionClass::Throw
        );

        let raise = Raise::throw(ExitReason::atom("boom")).with_stacktrace(sample_stacktrace());
        assert_eq!(raise.stacktrace.len(), 2);
        assert!(!raise.stacktrace.is_empty());
        assert_eq!(raise.to_string(), "throw boom");
    }

    #[test]
    fn stacktrace_display_and_frame_defaults() {
        assert_eq!(StackTrace::new().to_string(), "");
        assert_eq!(StackTrace::default(), StackTrace::new());

        let stacktrace = sample_stacktrace();
        assert_eq!(
            stacktrace.to_string(),
            "lists:map/2\nerlang:apply/3 (lists.erl:42)"
        );
        let frame = StackFrame::new("erlang", "apply", 3);
        assert_eq!(frame.to_string(), "erlang:apply/3");
        assert!(frame.location.is_none());
        assert_eq!(
            SourceLocation {
                file: AtomName::new("x.erl"),
                line: 7,
            }
            .to_string(),
            "x.erl:7"
        );
    }

    #[test]
    fn exit_reason_display_and_atoms() {
        assert_eq!(ExitReason::Normal.to_string(), "normal");
        assert_eq!(ExitReason::Shutdown.to_string(), "shutdown");
        assert_eq!(ExitReason::NoProc.to_string(), "noproc");
        assert_eq!(ExitReason::NoConnection.to_string(), "noconnection");
        assert_eq!(ExitReason::Killed.to_string(), "killed");
        assert_eq!(ExitReason::Aborted.to_string(), "aborted");
        assert_eq!(ExitReason::atom("boom").to_string(), "boom");
        assert_eq!(ExitReason::Structured.to_string(), "<structured reason>");

        assert_eq!(ExitReason::Normal.as_atom(), Some("normal"));
        assert_eq!(ExitReason::Shutdown.as_atom(), Some("shutdown"));
        assert_eq!(ExitReason::NoProc.as_atom(), Some("noproc"));
        assert_eq!(ExitReason::NoConnection.as_atom(), Some("noconnection"));
        assert_eq!(ExitReason::Killed.as_atom(), Some("killed"));
        assert_eq!(ExitReason::Aborted.as_atom(), Some("aborted"));
        assert_eq!(ExitReason::atom("weird").as_atom(), Some("weird"));
        assert_eq!(ExitReason::Structured.as_atom(), None);
    }

    #[test]
    fn exception_class_names() {
        assert_eq!(ExceptionClass::Error.as_str(), "error");
        assert_eq!(ExceptionClass::Exit.as_str(), "exit");
        assert_eq!(ExceptionClass::Throw.as_str(), "throw");
    }

    #[test]
    fn atom_name_conversions() {
        let borrowed = AtomName::new("lists");
        assert_eq!(borrowed.as_str(), "lists");
        assert_eq!(borrowed.to_string(), "lists");
        assert_eq!(AsRef::<str>::as_ref(&borrowed), "lists");

        let from_static: AtomName = "erlang".into();
        assert_eq!(from_static, AtomName::new("erlang"));
        let owned: AtomName = String::from("dynamic").into();
        assert_eq!(owned.as_str(), "dynamic");

        let cow: AtomName = Cow::Borrowed("cow").into();
        assert_eq!(cow.as_str(), "cow");
        assert_eq!(cow, AtomName::new(Cow::Owned(String::from("cow"))));
    }

    #[test]
    fn live_reason_round_trips_and_type_tests() {
        let number = LiveReason::new(7u64);
        assert_eq!(number.downcast_ref::<u64>(), Some(&7));
        assert!(number.downcast_ref::<String>().is_none());
        assert!(number.as_any().is::<u64>());

        let text = LiveReason::new(String::from("case_clause"));
        assert_eq!(
            text.downcast_ref::<String>().map(String::as_str),
            Some("case_clause")
        );
        assert!(text.downcast_ref::<u64>().is_none());
        assert!(text.as_any().is::<String>());
    }

    #[test]
    fn raise_carries_live_reason_and_clone_preserves_it() {
        assert!(Raise::error(ExitReason::Structured).live_reason().is_none());

        let raise = Raise::error(ExitReason::Structured)
            .with_live_reason(LiveReason::new(String::from("case_clause")));
        assert_eq!(
            raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<String>())
                .map(String::as_str),
            Some("case_clause")
        );

        let clone = raise.clone();
        assert_eq!(
            clone
                .live_reason()
                .and_then(|live| live.downcast_ref::<String>())
                .map(String::as_str),
            Some("case_clause")
        );
        // Cloning shares the payload rather than copying it.
        assert_eq!(
            raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<String>()),
            clone
                .live_reason()
                .and_then(|live| live.downcast_ref::<String>())
        );
    }

    #[test]
    fn raise_equality_ignores_live_reason_but_keeps_observable_parts() {
        let plain = Raise::error(ExitReason::Structured);
        let with_slot =
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(1u64));
        let other_slot =
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(2u64));
        assert_eq!(plain, with_slot);
        assert_eq!(with_slot, other_slot);

        // The class still distinguishes.
        assert_ne!(
            Raise::error(ExitReason::Structured),
            Raise::throw(ExitReason::Structured)
        );
        // The reason still distinguishes.
        assert_ne!(
            Raise::error(ExitReason::Normal),
            Raise::error(ExitReason::Shutdown)
        );
        // The stacktrace still distinguishes.
        assert_ne!(
            Raise::throw(ExitReason::atom("boom")),
            Raise::throw(ExitReason::atom("boom")).with_stacktrace(sample_stacktrace())
        );
    }

    #[test]
    fn raise_debug_hides_the_live_reason_and_display_is_unchanged() {
        assert_eq!(format!("{:?}", LiveReason::new(1u64)), "<live reason>");

        let raise = Raise::error(ExitReason::Structured)
            .with_live_reason(LiveReason::new(String::from("secret-payload")));
        let debug = format!("{raise:?}");
        assert!(!debug.contains("secret-payload"));
        assert!(debug.contains("<live reason>"));
        assert!(debug.contains("Structured"));
        assert_eq!(raise.to_string(), "error <structured reason>");
    }

    #[test]
    fn structured_raise_with_live_reason_keeps_atom_behavior() {
        let raise = Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(42u64));
        assert_eq!(raise.reason.as_atom(), None);
        let error = BeamError::Raise(raise);
        assert!(error.is_raise());
        assert_eq!(error.as_atom(), None);
        assert_eq!(error.to_string(), "error <structured reason>");
    }
}
