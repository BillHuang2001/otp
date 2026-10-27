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
//! Crash reports — the `=ERROR REPORT====` output for an **abnormal process termination**.
//!
//! # What ERTS reports, and what it does not
//!
//! The emulator logs one report per process that dies from an **uncaught exception of the
//! `error` class**, and nothing else (`terminate_proc`,
//! `../../erts/emulator/beam/beam_common.c:686-717`, guarded by `c_p->freason & EXF_LOG`).
//! `EXF_LOG` is a field of `EXC_ERROR` and of no other primary exception
//! (`../../erts/emulator/beam/error.h:73-114`), so:
//!
//! * an `error` raise (`erlang:error/1,2,3`, `badarith`, `badmatch`, `{badkey, K}`, …) reports;
//! * an uncaught **`throw`** reports too, because a throw that reaches the top of the stack is
//!   *promoted* to the error `{nocatch, Value}` before the check (`beam_common.c:508-516`);
//! * an **`exit`**-class reason never reports: plain `exit(R)`, `normal`, `shutdown`, `killed`
//!   — and therefore also every termination a **link**, a **monitor** or an `exit/2` causes,
//!   which carries an exit reason but no exception at all;
//! * a trapped `{'EXIT', …}` message is not a termination, so the trapping process reports
//!   nothing (the dying peer still reports under its own reason).
//!
//! # The shape (byte-exact against the oracle)
//!
//! ```text
//! =ERROR REPORT==== 27-Oct-2026::21:01:05.758635 ===
//! Error in process <0.83.0> with exit value:
//! {my_boom,[{crash1,nested,0,[{file,"crash1.erl"},{line,4}]}]}
//!
//! ```
//!
//! The header is `logger_formatter`'s **legacy** header (`legacy_header => true`, the default
//! handler's configuration): `"=~ts==== ~w-~s-~4w::~2..0w:~2..0w:~2..0w.~6..0w ~s==="` with
//! the title `"ERROR REPORT"` (`lib/kernel/src/logger_formatter.erl:565-574`) — the day is
//! **unpadded** (`~w`), the month is the three-letter abbreviation and the time has microsecond
//! precision. The message is the emulator's own format string, `"Error in process ~p "` +
//! `"on node ~p "` (on a **distributed** node only) + `"with exit value:~n~p~n"`
//! (`beam_common.c:700-705`), and the formatter's template appends one more newline, which is
//! the blank line that ends the block.
//!
//! The exit value is a **term**: `{Reason, Stacktrace}` for an `error`, `{{nocatch, Reason},
//! Stacktrace}` for a `throw`. Each stacktrace item is `{Module, Function, Arity | Args,
//! Location}` — this runtime always emits the **arity** form (the argument-list form comes from
//! `erlang:error/2,3`, which supplies an explicit arglist; see *Known gaps*), and `Location` is
//! `[{file, File}, {line, N}]` or `[]` when the frame has no source position
//! (`init:start_it/1` renders `[]` in the oracle for exactly that reason).
//!
//! # Reuse, not a second printer
//!
//! The exit value is built as a real term and rendered by [`Runtime::format_term`] — the
//! `io:format/2` `~p` printer every other human-facing string in this crate goes through — so
//! the `~p` printable-string rule, the 80-column wrapping of the `~p` control and the escaping
//! of atoms/strings are the BIF's, never re-implemented here. The term is built on a
//! **private** [`Heap`] because the dying process's own heap is about to go away and the
//! printer's scratch region is reset for every call.
//!
//! # Known gaps (honest, never a fabricated report)
//!
//! * **No `logger` handler configuration.** This runtime has no `logger` process, no
//!   `logger:add_handler/3` and no `logger_formatter` options, so the report always uses the
//!   default handler's shape and always goes to **stdout** (`standard_io`, which is where the
//!   oracle's default `logger_std_h` handler writes it).
//! * **No SASL reports**: `supervisor`/`gen_server` terminate reports
//!   (`{application, …}`/`{supervisor, …}`) are Erlang-level `logger` calls in
//!   `lib/kernel`/`lib/stdlib`; this runtime only emits the *emulator's* report.
//! * **The timestamp is UTC.** The oracle renders `erlang:universaltime_to_localtime/1`; this
//!   runtime has no timezone database, so it renders UTC, which is byte-identical to the oracle
//!   under `TZ=UTC`.
//! * **`erlang:error(Reason, Args[, Opts])`'s explicit arglist** and its `error_info` entry are
//!   not reproduced: the stacktrace item always carries the arity.
//! * **The `max_heap_size` report** (`MAX_HEAP_SIZE_LOG`'s
//!   `"Process ~p … max heap size …"` message, `../../erts/emulator/beam/erl_gc.c:3770-3795`)
//!   is not emitted: a heap-limit kill is silent here.
use std::time::{SystemTime, UNIX_EPOCH};
use beam_alloc::Heap;
use beam_common::{BeamError, ExceptionClass, Result, StackFrame};
use beam_process::ProcessId;
use beam_term::{Eterm, NIL, make_small};
use crate::crash_dump::{MONTH_ABBREV, civil_from_days};
use crate::runtime::Runtime;
/// Renders the crash report for the abnormal termination of `pid`, or `None` when ERTS would
/// stay silent.
///
/// `reason` is the process's **exit-reason term** — the same term the termination carries and
/// the process table propagates ([`crate::runner::exit_reason`] produced it), so the report can
/// never disagree with the observable exit reason about *what* the reason was.
///
/// `None` is answered for the three silent shapes: an `exit`-class raise (which is why a link,
/// a monitor, an `exit/2` and a trapped `{'EXIT', …}` never report), and the process the CLI's
/// `run` action is itself waiting for — that one's abnormal exit *is* the action's result and
/// the CLI reports it on stderr (`beam_rt: error …`), exactly as the oracle leaves the
/// equivalent `-eval`/`-s` invocation silent.
///
/// # Errors
///
/// The heap's allocation failure while building the exit-value term and whatever the `~p`
/// printer reports. A caller that cannot write the report must not turn it into the
/// termination's own outcome (see [`crate::runner`]).
pub(crate) fn crash_report(
    runtime: &mut Runtime,
    pid: ProcessId,
    error: &BeamError,
    reason: Eterm,
) -> Result<Option<String>> {
    let Some((class, frames)) = reportable(error) else {
        return Ok(None);
    };
    if runtime.is_run_target(pid) {
        return Ok(None);
    }
    let rendered = render_exit_value(runtime, class, reason, frames)?;
    let mut text = String::with_capacity(80 + rendered.len());
    text.push_str(&header_line(SystemTime::now()));
    text.push('\n');
    text.push_str("Error in process ");
    text.push_str(&pid_text(pid));
    if let Some(node) = runtime.node_name() {
        text.push_str(" on node ");
        text.push_str(node);
    }
    text.push_str(" with exit value:\n");
    text.push_str(&rendered);
    // The formatter's template adds the newline that ends the block; the message itself already
    // ends with one (`"…~n~p~n"`), so this is the oracle's trailing blank line.
    text.push('\n');
    Ok(Some(text))
}
/// Writes a crash report for `pid` to the runtime's report stream (stdout) as the default
/// `logger` handler would.
///
/// The `Err` case is **not** propagated into the termination by design: a report that cannot be
/// written must never replace the exit outcome it describes, so a failure is surfaced through
/// the env-gated [`crate::debug_trace`] channel instead.
pub(crate) fn report_crash(runtime: &mut Runtime, pid: ProcessId, reason: Eterm, error: &BeamError) {
    use beam_bifs::OutputSink;
    match crash_report(runtime, pid, error, reason) {
        Ok(Some(text)) => {
            if let Err(error) = beam_bifs::StdoutSink.write_str(&text) {
                crate::debug_trace::trace(format_args!(
                    "crash report for {pid:?} could not be written: {error:?}"
                ));
            }
        }
        Ok(None) => {}
        Err(error) => crate::debug_trace::trace(format_args!(
            "crash report for {pid:?} could not be built: {error:?}"
        )),
    }
}
/// The class and frames of an exception that ERTS would report, or `None` for a silent one.
///
/// A failure that is **not** a raise (runtime plumbing such as `TermNotFound`, or an `io`
/// failure inside a BIF) has no class of its own; it is reported as an `error` with no frames
/// rather than hidden, because the process is still terminating abnormally.
fn reportable(error: &BeamError) -> Option<(ExceptionClass, &[StackFrame])> {
    match error {
        BeamError::Raise(raise) => match raise.class {
            // The C's `EXF_LOG` is clear for `EXTAG_EXIT`: `exit(R)`, `normal`, `shutdown` and
            // every link/monitor propagation are silent.
            ExceptionClass::Exit => None,
            class => Some((class, raise.stacktrace.frames.as_slice())),
        },
        _ => Some((ExceptionClass::Error, &[])),
    }
}
/// The `=ERROR REPORT==== … ===` header, in UTC (see the module's *Known gaps*).
fn header_line(now: SystemTime) -> String {
    let elapsed = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs() as i64;
    let micros = elapsed.subsec_micros() % 1_000_000;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "=ERROR REPORT==== {day}-{}-{year}::{:02}:{:02}:{:02}.{micros:06} ===",
        MONTH_ABBREV[(month - 1) as usize],
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
/// Renders the exit value the report shows: `{Reason, Stacktrace}`, or
/// `{{nocatch, Reason}, Stacktrace}` for an uncaught `throw` (the C promotes it to an error with
/// that reason, `beam_common.c:508-516`).
///
/// The term is built on a private heap sized from the frame list — the dying process's own heap
/// is about to be released and the printer's scratch region is reset for every call, so neither
/// can hold it — and it is *rendered inside this function*: the heap owns the block the term
/// points into and releases it on return, so the term must never escape.
fn render_exit_value(
    runtime: &mut Runtime,
    class: ExceptionClass,
    reason: Eterm,
    frames: &[StackFrame],
) -> Result<String> {
    let mut heap = Heap::new(term_words(frames))?;
    let value = exit_value(runtime, &mut heap, class, reason, frames)?;
    runtime.format_term(value, None)
}
/// The exit-value **term** on `heap` (see [`render_exit_value`] for the lifetime rule).
fn exit_value(
    runtime: &mut Runtime,
    heap: &mut Heap,
    class: ExceptionClass,
    reason: Eterm,
    frames: &[StackFrame],
) -> Result<Eterm> {
    let nocatch = runtime.intern("nocatch");
    let file = runtime.intern("file");
    let line = runtime.intern("line");
    let mut trace = NIL;
    // The frames arrive innermost first — the C's `build_stacktrace` order — and a list is built
    // tail-first, so walk them back to front.
    for frame in frames.iter().rev() {
        let item = frame_term(runtime, heap, frame, file, line)?;
        trace = heap.cons(item, trace)?;
    }
    let reason = match class {
        ExceptionClass::Throw => heap.tuple(&[nocatch, reason])?,
        // `ExceptionClass` is `#[non_exhaustive]`: a class this runtime does not know is
        // reported with the reason it carries rather than reshaped.
        _ => reason,
    };
    heap.tuple(&[reason, trace])
}
/// One stacktrace item: `{Module, Function, Arity, Location}`.
fn frame_term(
    runtime: &mut Runtime,
    heap: &mut Heap,
    frame: &StackFrame,
    file: Eterm,
    line: Eterm,
) -> Result<Eterm> {
    let module = runtime.intern(frame.module.as_str());
    let function = runtime.intern(frame.function.as_str());
    let arity = make_small(i64::from(frame.arity));
    let location = match &frame.location {
        Some(location) => {
            let name = string_term(heap, location.file.as_str())?;
            let index = make_small(i64::from(location.line));
            let file_pair = heap.tuple(&[file, name])?;
            let line_pair = heap.tuple(&[line, index])?;
            let line_list = heap.cons(line_pair, NIL)?;
            heap.cons(file_pair, line_list)?
        }
        // The oracle's own shape for a frame with no source position (`{init,start_it,1,[]}`).
        None => NIL,
    };
    heap.tuple(&[module, function, arity, location])
}
/// An Erlang **string** (a list of small integers) for `text` — the shape the oracle's
/// `{file, "crash1.erl"}` uses.
fn string_term(heap: &mut Heap, text: &str) -> Result<Eterm> {
    let mut list = NIL;
    for character in text.chars().rev() {
        list = heap.cons(make_small(i64::from(u32::from(character))), list)?;
    }
    Ok(list)
}
/// A safe upper bound on the words [`exit_value`] needs for `frames`.
///
/// Per frame: the 4-tuple (5 words), its list cell (2), the location list's two cells (4), the
/// `{file, …}`/`{line, …}` tuples (6) and the file string (2 words per character plus the nil).
/// The constant covers the outer tuple and the `{nocatch, …}` tuple of a throw.
fn term_words(frames: &[StackFrame]) -> usize {
    let mut words = 32;
    for frame in frames {
        let file_len = frame
            .location
            .as_ref()
            .map_or(0, |location| location.file.as_str().chars().count());
        words += 17 + 2 * file_len;
    }
    words
}
/// Renders the local pid `<0.N.S>` exactly as `~p` of an internal pid does.
fn pid_text(pid: ProcessId) -> String {
    format!("<0.{}.{}>", pid.number(), pid.serial())
}
#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::{AtomName, ExitReason, Raise, SourceLocation, StackTrace};
    use crate::runtime::Runtime;
    use beam_process::ProcessId;
    /// A runtime with the shared atom table seeded (the report's atoms come from it).
    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }
    /// An `error` raise carrying one frame with a source position.
    fn error_with_frame(reason: &str) -> BeamError {
        let mut stacktrace = StackTrace::new();
        let mut frame = StackFrame::new("crash1", "nested", 0);
        frame.location = Some(SourceLocation {
            file: AtomName::new("crash1.erl"),
            line: 4,
        });
        stacktrace.push(frame);
        BeamError::Raise(
            Raise::error(ExitReason::atom(reason)).with_stacktrace(stacktrace),
        )
    }
    /// The report body after the header line (the timestamp is not part of any assertion).
    fn body(text: &str) -> String {
        let mut lines = text.lines();
        let header = lines.next().expect("a header line");
        assert!(
            header.starts_with("=ERROR REPORT==== "),
            "the legacy error_logger header: {header}"
        );
        assert!(header.ends_with(" ==="), "the header's closing marker: {header}");
        // `D-Mon-YYYY::HH:MM:SS.micro` — the shape, never the instant.
        let stamp = header
            .trim_start_matches("=ERROR REPORT==== ")
            .trim_end_matches(" ===");
        let (date, time) = stamp.split_once("::").expect("date::time");
        let (day, month, year) = date.split_once('-').expect("D-Mon-Y");
        assert!(day.parse::<u32>().is_ok(), "an unpadded day: {day}");
        assert_eq!(month.len(), 3, "the abbreviated month: {month}");
        assert_eq!(year.len(), 4, "a four-digit year: {year}");
        let (clock, micros) = time.split_once('.').expect("S.micros");
        assert_eq!(clock.split(':').count(), 3, "HH:MM:SS: {clock}");
        assert_eq!(micros.len(), 6, "microsecond precision: {micros}");
        lines.collect::<Vec<_>>().join("\n")
    }
    #[test]
    fn an_uncaught_error_reports_its_reason_and_stacktrace() {
        let mut rt = runtime();
        let error = error_with_frame("boom");
        let reason = rt.intern("boom");
        let pid = ProcessId::new(2, 1);
        let text = crash_report(&mut rt, pid, &error, reason)
            .expect("the report builds")
            .expect("an error-class termination reports");
        assert_eq!(
            body(&text),
            format!(
                "Error in process {} with exit value:\n\
                 {{boom,[{{crash1,nested,0,[{{file,\"crash1.erl\"}},{{line,4}}]}}]}}",
                pid_text(pid)
            )
        );
        assert!(text.ends_with('\n'), "the terminating newline: {text:?}");
    }
    #[test]
    fn an_uncaught_throw_is_reported_as_a_nocatch_error() {
        let mut rt = runtime();
        let error = BeamError::Raise(Raise::throw(ExitReason::atom("x")));
        let reason = rt.intern("x");
        let text = crash_report(&mut rt, ProcessId::new(2, 2), &error, reason)
            .expect("the report builds")
            .expect("an uncaught throw is promoted to an error and reports");
        assert_eq!(
            body(&text),
            "Error in process <0.2.2> with exit value:\n{{nocatch,x},[]}"
        );
    }
    #[test]
    fn an_exit_class_reason_is_silent() {
        let mut rt = runtime();
        let error = BeamError::Raise(Raise::exit(ExitReason::atom("boom")));
        let reason = rt.intern("boom");
        assert_eq!(crash_report(&mut rt, ProcessId::new(2, 3), &error, reason)
            .expect("nothing to build"), None);
        // `normal`, `shutdown` and `killed` are the same class — a link/monitor propagation and
        // an `exit/2` never report either.
        let error = BeamError::Raise(Raise::exit(ExitReason::Normal));
        assert_eq!(crash_report(&mut rt, ProcessId::new(2, 3), &error, reason)
            .expect("nothing to build"), None);
    }
    #[test]
    fn the_cli_route_target_is_silent() {
        let mut rt = runtime();
        let pid = ProcessId::new(3, 0);
        // What the `run` action does for the process it waits for.
        rt.set_run_wake_pid(Some(pid));
        let error = error_with_frame("boom");
        let reason = rt.intern("boom");
        assert_eq!(
            crash_report(&mut rt, pid, &error, reason)
                .expect("nothing to build"),
            None
        );
        // A *different* process crashing in the same run does report.
        assert!(crash_report(&mut rt, ProcessId::new(2, 9), &error, reason)
            .expect("the report builds")
            .is_some());
    }
    #[test]
    fn a_raise_without_frames_renders_an_empty_stacktrace() {
        let mut rt = runtime();
        let error = BeamError::Raise(Raise::error(ExitReason::atom("badarith")));
        let reason = rt.intern("badarith");
        let text = crash_report(&mut rt, ProcessId::new(2, 4), &error, reason)
            .expect("the report builds")
            .expect("an error-class termination reports");
        assert_eq!(
            body(&text),
            "Error in process <0.2.4> with exit value:\n{badarith,[]}"
        );
    }
    #[test]
    fn a_frame_without_a_source_position_renders_the_empty_location() {
        let mut rt = runtime();
        let mut stacktrace = StackTrace::new();
        stacktrace.push(StackFrame::new("init", "start_it", 1));
        let error = BeamError::Raise(
            Raise::error(ExitReason::atom("boom")).with_stacktrace(stacktrace),
        );
        let reason = rt.intern("boom");
        let text = crash_report(&mut rt, ProcessId::new(2, 5), &error, reason)
            .expect("the report builds")
            .expect("an error-class termination reports");
        assert_eq!(
            body(&text),
            "Error in process <0.2.5> with exit value:\n{boom,[{init,start_it,1,[]}]}"
        );
    }
    #[test]
    fn a_structured_reason_keeps_its_shape_and_a_bitstring_frame_file_is_a_string() {
        let mut rt = runtime();
        // The reason is rendered from the term the termination carries, never re-derived from
        // the error, so a structured reason keeps whatever the process built.
        let reason = {
            let badkey = rt.intern("badkey");
            let key = rt.intern("nokey");
            let mut heap = Heap::new(8).expect("a scratch heap");
            heap.tuple(&[badkey, key]).expect("the reason tuple")
        };
        let mut stacktrace = StackTrace::new();
        let mut frame = StackFrame::new("maps", "get", 2);
        frame.location = Some(SourceLocation {
            file: AtomName::new("maps.erl"),
            line: 12,
        });
        stacktrace.push(frame);
        let error = BeamError::Raise(
            Raise::error(ExitReason::Structured).with_stacktrace(stacktrace),
        );
        let text = crash_report(&mut rt, ProcessId::new(2, 6), &error, reason)
            .expect("the report builds")
            .expect("an error-class termination reports");
        assert_eq!(
            body(&text),
            "Error in process <0.2.6> with exit value:\n\
             {{badkey,nokey},[{maps,get,2,[{file,\"maps.erl\"},{line,12}]}]}"
        );
    }
    #[test]
    fn a_plain_runtime_omits_the_node_half_of_the_message() {
        let mut rt = runtime();
        let error = error_with_frame("boom");
        let reason = rt.intern("boom");
        // Not a node: the oracle omits the `on node …` half entirely.
        let text = crash_report(&mut rt, ProcessId::new(2, 7), &error, reason)
            .expect("the report builds")
            .expect("an error-class termination reports");
        assert!(!text.contains(" on node "), "no node half: {text}");
    }
}
