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

//! The `io` **device** floor: `io:put_chars/1,2`, `io:nl/0,1`, `io:fwrite/1,2,3`,
//! `io:write/1,2` and `io:format/3`.
//!
//! # Why these are floor keys and not `lib/stdlib/src/io.erl` bodies
//!
//! Every one of them is an `io.erl` wrapper over `o_request/2` (`io.erl:160-207`), which
//! resolves the request's **device** and posts `{io_request, self(), Mref, Request}` to it,
//! blocking in a `receive` for `{io_reply, Mref, Reply}`. The default device is
//! `group_leader()` (`io.erl:196-200`, `default_output/0`), and this runtime's group leader
//! for its own root process is **itself**, so `execute_request/3`'s first clause
//! (`io.erl:1653`) answers `{ErrorTag, calling_self}` and the wrapper raises
//! `error:calling_self` (`conv_reason/1`, `io.erl:798`) — every `io:fwrite/2` in the
//! installed **compiler**'s error-reporting `catch` handlers
//! (`lib/compiler/src/beam_ssa_bool.erl:140-152`, `beam_ssa_type.erl:225-233`) therefore
//! *replaces* the compiler's real diagnostic with `calling_self`.
//!
//! Serving the keys here removes `io.erl` from the path for them, so the request never
//! leaves the runtime and the device question is answered by [`RuntimeContext::io_device`]
//! instead of by a message round trip this runtime cannot complete against itself.
//!
//! # The device model
//!
//! [`RuntimeContext::io_stream`] resolves the device argument exactly as `o_request/2` +
//! `request/3` do (probed against the oracle, OTP 29.1.1 / erts-17.1):
//!
//! | Device | Oracle | Here |
//! |---|---|---|
//! | omitted (`io:put_chars/1`, `io:nl/0`, `io:fwrite/1,2`, `io:write/1`, `io:format/1,2`) | the caller's **group leader** | the caller's output stream (stdout) |
//! | `standard_io` | `request/3`'s `standard_io` clause rewrites it to `group_leader()` | stdout |
//! | `user` | the registered terminal device | stdout |
//! | `standard_error` | the registered stderr device | stderr |
//! | the caller's **own** group-leader pid | that device (the terminal) | stdout |
//! | the registered `standard_error` pid | that device | stderr |
//! | another **registered** device pid | the `{io_request, …}` round trip | the typed `unsupported` (a BIF cannot block) |
//! | an **unregistered** atom | `{ErrorTag, arguments}` → `error:badarg` | `error:badarg` |
//! | not an atom and not a pid | no `o_request/2` clause matches → `error:function_clause` | `error:function_clause` |
//!
//! A non-stdout device that this runtime *can* serve is never silently redirected to
//! stdout, and a device it cannot reach is a typed `unsupported` rather than a fabricated
//! `ok`.
//!
//! # Reused machinery
//!
//! `io:fwrite/1,2,3` and `io:format/3` render through the **registered** `io_lib:format/2`
//! BIF — the very implementation `io.erl`'s server calls for the `{format, …}` request
//! (`io_request/2`, `io.erl:1753`) — and `io:write/1,2` through the same BIF with the `~w`
//! control (`io_lib:write/1` is `write1(Term, -1, latin1, undefined)`, whose fallback for a
//! non-immediate is `io_lib_format:fwrite("~w", [Term])`; probed **equal** to
//! `io_lib:format("~w", [Term])` for every term kind this runtime builds, including char
//! lists, which `~p` would render as a string and `write/1` does not). `io:nl/0,1` writes
//! the one newline `io_lib:nl/0` is (`io_request/2`'s `nl` clause), and `io:put_chars/1,2`
//! flattens its character data (`{put_chars, unicode, Chars}`'s payload) the same way the
//! output sink does.
//!
//! [`RuntimeContext::io_device`]: RuntimeContext
//! [`RuntimeContext::io_stream`]: RuntimeContext

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_process::ProcessId;
use beam_term::{atom_val, is_atom, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::error;
use crate::native::prim;

/// The output stream an `io:*` request writes to.
///
/// This runtime has exactly two: the process's **output stream** (the `beam_bifs`
/// [`OutputSink`](beam_bifs::OutputSink) — stdout unless a caller installed a capture sink)
/// and the process's **standard error** (fd 2, the stream the registered `standard_error`
/// device writes to: `prim_tty`'s `stderr` handle, `src/native/prim_tty.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// The caller's output stream (`write_output`).
    Output,
    /// The process's standard error.
    Stderr,
}

/// The class of an `io:*` **device** argument, as resolved by
/// [`RuntimeContext::io_device`].
///
/// Mirrors what `io.erl`'s `o_request/2` + `request/3` would do with the argument, without
/// the message round trip this runtime cannot perform from a BIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Device {
    /// The device selects this stream.
    Stream(Stream),
    /// The argument is an atom naming **no registered process**: `request/3`'s
    /// `whereis(Name) =:= undefined` clause answers `{ErrorTag, arguments}`, which
    /// `o_request/2` turns into `error:badarg` (`conv_reason/1`, `io.erl:782`).
    UnknownAtom,
    /// The argument is neither an atom nor a pid, so no `o_request/2` clause matches and
    /// the oracle raises `error:function_clause`.
    NoClause,
    /// A **live** io device this runtime cannot serve from a BIF: `io.erl` would post an
    /// `{io_request, …}` to it and block in a `receive` for the reply, which a BIF may not
    /// do.
    Unreachable,
}

/// The typed capability gap for a live device that needs the `{io_request, …}` round trip.
const UNREACHABLE_DEVICE: &str = "io: an explicit io-device process (only `standard_io`/`user` on \
                                   the caller's output stream and `standard_error` are served)";

impl RuntimeContext<'_> {
    /// `io:put_chars/1,2` — write the characters `Chars` to the device and answer `ok`.
    ///
    /// `which` is the arity. `/1` is `o_request(default_output(), {put_chars, unicode, Chars})`
    /// (`default_output/0` is `group_leader()`) and `/2` takes the device **first**
    /// (`io.erl:160-207`), so the argument is resolved by exactly the same
    /// [`Self::io_stream`] the other device-taking arms use — a device this runtime serves
    /// selects a stream, and one it cannot reach is the oracle's own raise (never a silent
    /// redirect to stdout).
    pub(super) fn io_put_chars(&mut self, args: &[Eterm], which: usize) -> Result<Eterm> {
        let (device, chars) = match which {
            1 => (None, *args.first().ok_or(BeamError::BadArg)?),
            2 => (
                Some(*args.first().ok_or(BeamError::BadArg)?),
                *args.get(1).ok_or(BeamError::BadArg)?,
            ),
            _ => return Err(BeamError::BadArg),
        };
        // The device is resolved *before* the payload is read, exactly as `o_request/2` +
        // `request/3` do: both `io:put_chars(foo, …)` and `io:put_chars(1, …)` raise the
        // device's own reason whatever the character data is.
        let stream = self.io_stream(device)?;
        let text = self.chars_to_string(chars)?;
        self.write_stream(stream, &text)?;
        self.atom("ok")
    }

    /// `io:nl/0` / `io:nl/1` — write one newline to the device and answer `ok`.
    ///
    /// `which` is the arity. The newline is the one `io_lib:nl/0` returns (`"\n"`), which
    /// `io_request/2`'s `nl` clause sends as the request's payload.
    pub(super) fn io_nl(&mut self, args: &[Eterm], which: usize) -> Result<Eterm> {
        let device = match which {
            0 => None,
            1 => Some(*args.first().ok_or(BeamError::BadArg)?),
            _ => return Err(BeamError::BadArg),
        };
        let stream = self.io_stream(device)?;
        self.write_stream(stream, "\n")?;
        self.atom("ok")
    }

    /// `io:fwrite/1,2,3` — render `Format` with `Data` on the device and answer `ok`.
    ///
    /// `which` is the arity. `io:fwrite` and `io:format` are the *same* request in
    /// `io.erl` (`o_request/2` builds `{format, Format, Args}` for both), so
    /// [`Self::io_format_3`] is the `/3` form of this one.
    pub(super) fn io_fwrite(&mut self, args: &[Eterm], which: usize) -> Result<Eterm> {
        let (device, format, data) = match which {
            // `fwrite(Format)` is `fwrite(standard_io, Format, [])` (`io.erl:794-804`).
            1 => (
                None,
                *args.first().ok_or(BeamError::BadArg)?,
                NIL,
            ),
            2 => (
                None,
                *args.first().ok_or(BeamError::BadArg)?,
                *args.get(1).ok_or(BeamError::BadArg)?,
            ),
            3 => (
                Some(*args.first().ok_or(BeamError::BadArg)?),
                *args.get(1).ok_or(BeamError::BadArg)?,
                *args.get(2).ok_or(BeamError::BadArg)?,
            ),
            _ => return Err(BeamError::BadArg),
        };
        self.io_emit_format(device, format, data)
    }

    /// `io:format/3` — the `{format, Format, Args}` request of [`Self::io_fwrite`].
    pub(super) fn io_format_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        self.io_fwrite(args, 3)
    }

    /// `io:write/1,2` — write `Term`'s `~w` representation to the device and answer `ok`.
    ///
    /// `which` is the arity (`io:write/2` takes the device first).
    pub(super) fn io_write(&mut self, args: &[Eterm], which: usize) -> Result<Eterm> {
        let (device, term) = match which {
            1 => (None, *args.first().ok_or(BeamError::BadArg)?),
            2 => (
                Some(*args.first().ok_or(BeamError::BadArg)?),
                *args.get(1).ok_or(BeamError::BadArg)?,
            ),
            _ => return Err(BeamError::BadArg),
        };
        let stream = self.io_stream(device)?;
        // `io_lib:write(Term)` is `io_lib:format("~w", [Term])` (see the module docs).
        let control = self.charlist("~w")?;
        let data = terms::make_list(self, &[term])?;
        let rendered = self.call_bif("io_lib", "format", 2, &[control, data])?;
        let text = self.chars_to_string(rendered)?;
        self.write_stream(stream, &text)?;
        self.atom("ok")
    }

    /// The `{format, Format, Data}` request: render through the registered
    /// `io_lib:format/2`, write the flattened text to `device`'s stream, answer `ok`.
    fn io_emit_format(&mut self, device: Option<Eterm>, format: Eterm, data: Eterm) -> Result<Eterm> {
        let stream = self.io_stream(device)?;
        let rendered = self.call_bif("io_lib", "format", 2, &[format, data])?;
        let text = self.chars_to_string(rendered)?;
        self.write_stream(stream, &text)?;
        self.atom("ok")
    }

    /// The stream a request's **effective device** selects, or the raise the oracle
    /// produces for a device this runtime cannot serve.
    ///
    /// `None` is the omitted (default) device: `default_output/0` is `group_leader()`, which
    /// this runtime's own single output stream stands in for (the group leader of a process
    /// this runtime starts is itself, which is exactly the `calling_self` case this floor
    /// exists to answer).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for an atom naming no registered process, the bare
    /// `function_clause` for a non-atom/non-pid argument, and the typed `unsupported` for a
    /// live device that needs the `{io_request, …}` round trip — each the oracle's own
    /// answer, never a silent redirect to stdout.
    fn io_stream(&self, device: Option<Eterm>) -> Result<Stream> {
        let Some(device) = device else {
            return Ok(Stream::Output);
        };
        match self.io_device(device) {
            Device::Stream(stream) => Ok(stream),
            Device::UnknownAtom => Err(BeamError::BadArg),
            Device::NoClause => Err(prim::function_clause()),
            Device::Unreachable => Err(error::unsupported(UNREACHABLE_DEVICE)),
        }
    }

    /// Classifies an `io:*` device argument (`o_request/2` + `request/3`'s device rules).
    ///
    /// The three **kernel io atoms** are recognised by name — `standard_io` (which
    /// `request/3` rewrites to `group_leader()`), `user` and `standard_error` — because they
    /// are this runtime's own two streams regardless of what the name registry currently
    /// holds. Every other atom is resolved through the registry
    /// ([`ProcessTable::whereis`](beam_process::ProcessTable::whereis)), so an unregistered
    /// atom is [`Device::UnknownAtom`] (the oracle's `error:badarg`) and a registered one is
    /// classified by pid.
    fn io_device(&self, device: Eterm) -> Device {
        if is_atom(device) {
            match self.runtime.code_server.atoms().name(atom_val(device)) {
                Some("standard_io") | Some("user") => return Device::Stream(Stream::Output),
                Some("standard_error") => return Device::Stream(Stream::Stderr),
                _ => {}
            }
            return match self.process_table().whereis(atom_val(device)) {
                Some(pid) => self.io_pid_device(pid),
                None => Device::UnknownAtom,
            };
        }
        match ProcessId::from_eterm(device) {
            Ok(pid) => self.io_pid_device(pid),
            // Not an atom and not a pid: no `o_request/2` clause matches.
            Err(_) => Device::NoClause,
        }
    }

    /// The stream a **live** device pid selects.
    ///
    /// The caller's own **group leader** is the default device — the terminal this runtime
    /// writes to — and the registered `standard_error` device is the process's standard
    /// error. Every other live device would need the `{io_request, …}`/`{io_reply, …}` round
    /// trip, which a BIF cannot perform ([`Device::Unreachable`]).
    fn io_pid_device(&self, pid: ProcessId) -> Device {
        if pid == self.runtime.group_leader_of(self.pid()) {
            return Device::Stream(Stream::Output);
        }
        let standard_error = self
            .runtime
            .code_server
            .atoms()
            .lookup("standard_error")
            .and_then(|name| self.process_table().whereis(name));
        if standard_error == Some(pid) {
            return Device::Stream(Stream::Stderr);
        }
        Device::Unreachable
    }

    /// Writes `text` to `stream`: the caller's output sink for [`Stream::Output`], and the
    /// process's standard error for [`Stream::Stderr`].
    ///
    /// The stderr write is the same destination `prim_tty`'s `stderr` handle gives the
    /// registered `standard_error` device (`src/native/prim_tty.rs`), so the bytes land where
    /// the oracle's `io:format(standard_error, …)` puts them — never on stdout.
    fn write_stream(&mut self, stream: Stream, text: &str) -> Result<()> {
        match stream {
            Stream::Output => self.write_output(text),
            Stream::Stderr => {
                use std::io::Write;
                std::io::stderr()
                    .write_all(text.as_bytes())
                    .map_err(BeamError::Io)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime};
    use crate::runtime::Runtime;
    use beam_engine::ExecutionContext;
    use beam_term::make_small;

    /// Runs `io:<function>/<arity>` with `args` against a capture sink, answering the
    /// result plus what the call wrote to the caller's output stream.
    fn call_capturing(
        runtime: &mut Runtime,
        function: &str,
        args: &[Eterm],
    ) -> (Result<Eterm>, String) {
        let mut sink = beam_bifs::BytesSink::new();
        let result = {
            let mut context = RuntimeContext::with_output(runtime, &mut sink);
            context.call_bif("io", function, args.len() as u32, args)
        };
        (result, sink.as_str())
    }

    /// The empty argument list `io:fwrite/1`'s request supplies (`{format, Format, []}`).
    fn no_args(runtime: &mut Runtime) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, &[]).expect("the empty list")
    }

    /// `io:nl/0` writes exactly one newline and answers `ok`; `io:nl/1` does the same for
    /// every device this runtime serves.
    #[test]
    fn nl_writes_a_newline_for_the_default_and_served_devices() {
        let mut runtime = runtime();
        let (result, out) = call_capturing(&mut runtime, "nl", &[]);
        assert_eq!(runtime.atom_name_of(result.expect("nl/0")), Some("ok"));
        assert_eq!(out, "\n");

        for name in ["standard_io", "user"] {
            let device = atom(&mut runtime, name);
            let (result, out) = call_capturing(&mut runtime, "nl", &[device]);
            assert_eq!(
                runtime.atom_name_of(result.expect("nl/1")),
                Some("ok"),
                "nl({name})"
            );
            assert_eq!(out, "\n", "nl({name}) writes to the output stream");
        }
    }

    /// `io:put_chars/1` writes its characters to the default device (the caller's output
    /// stream, standing in for `group_leader()`) and answers `ok`; `/2` writes to every
    /// device this runtime serves.
    #[test]
    fn put_chars_writes_to_the_resolved_device() {
        let mut runtime = runtime();
        let text = chars(&mut runtime, "hi");
        let (result, out) = call_capturing(&mut runtime, "put_chars", &[text]);
        assert_eq!(
            runtime.atom_name_of(result.expect("put_chars/1")),
            Some("ok")
        );
        assert_eq!(out, "hi");

        for name in ["standard_io", "user"] {
            let mut served = crate::native::test_support::runtime();
            let device = atom(&mut served, name);
            let text = chars(&mut served, "hi");
            let (result, out) = call_capturing(&mut served, "put_chars", &[device, text]);
            assert_eq!(
                served.atom_name_of(result.expect("put_chars/2")),
                Some("ok"),
                "put_chars({name}, ...)"
            );
            assert_eq!(out, "hi", "put_chars({name}, ...) writes to the output stream");
        }
    }

    /// `io:put_chars/2`'s device is resolved by the **same** model as the other arms: an
    /// unregistered atom is the oracle's `badarg`, a non-atom/non-pid is the bare
    /// `function_clause`, and a live non-stdio device is the typed `unsupported` — never a
    /// silent redirect to stdout.
    #[test]
    fn put_chars_honours_the_device_rules() {
        let mut unknown_atom = runtime();
        let device = atom(&mut unknown_atom, "no_such_io_device");
        let text = chars(&mut unknown_atom, "x");
        let error = call(&mut unknown_atom, "io", "put_chars", &[device, text])
            .expect_err("put_chars/2");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");

        let mut no_clause = runtime();
        let text = chars(&mut no_clause, "x");
        let error = call(&mut no_clause, "io", "put_chars", &[make_small(1), text])
            .expect_err("put_chars/2");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");

        // A pid that is neither the caller's group leader nor the `standard_error` device:
        // `io.erl` would post an `{io_request, …}` and block, which a BIF may not do.
        let mut live = runtime();
        let own = {
            let mut sink = beam_bifs::NullSink;
            let context = RuntimeContext::with_output(&mut live, &mut sink);
            context.pid()
        };
        let device = ProcessId::new(own.number() + 1000, own.serial()).to_eterm();
        let text = chars(&mut live, "x");
        let error = call(&mut live, "io", "put_chars", &[device, text]).expect_err("put_chars/2");
        let reason = crate::error::unsupported_reason(&error).unwrap_or_else(|| {
            panic!("a live non-stdio device must be the typed capability gap: {error}")
        });
        assert!(reason.contains("io-device"), "{reason}");
    }

    /// `io:fwrite/1,2` renders `Format` with the registered `io_lib:format/2` and writes the
    /// text; `/1` supplies the oracle's empty argument list.
    #[test]
    fn fwrite_renders_through_io_lib_format() {
        let mut runtime = runtime();
        let format = chars(&mut runtime, "x~n");
        let (result, out) = call_capturing(&mut runtime, "fwrite", &[format]);
        assert_eq!(runtime.atom_name_of(result.expect("fwrite/1")), Some("ok"));
        assert_eq!(out, "x\n");

        let mut runtime = crate::native::test_support::runtime();
        let format = chars(&mut runtime, "~p~n");
        let data = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[make_small(7)]).expect("a list")
        };
        let (result, out) = call_capturing(&mut runtime, "fwrite", &[format, data]);
        assert_eq!(runtime.atom_name_of(result.expect("fwrite/2")), Some("ok"));
        assert_eq!(out, "7\n");
    }

    /// `io:write/1` writes `io_lib:write/1`'s rendering: a char list is a **list of
    /// integers** (not the string `~p` would print), an atom keeps its quotes.
    #[test]
    fn write_renders_with_the_w_control() {
        let mut runtime = runtime();
        let text = chars(&mut runtime, "abc");
        let (result, out) = call_capturing(&mut runtime, "write", &[text]);
        assert_eq!(runtime.atom_name_of(result.expect("write/1")), Some("ok"));
        assert_eq!(out, "[97,98,99]");

        let mut runtime = crate::native::test_support::runtime();
        let quoted = atom(&mut runtime, "hello world");
        let (result, out) = call_capturing(&mut runtime, "write", &[quoted]);
        assert_eq!(runtime.atom_name_of(result.expect("write/1")), Some("ok"));
        assert_eq!(out, "'hello world'");
    }

    /// `io:format/3` is `io:fwrite/3`'s request, and `/3`'s device argument is honoured for
    /// the streams this runtime serves.
    #[test]
    fn format_3_serves_the_explicit_device() {
        let mut runtime = runtime();
        let device = atom(&mut runtime, "standard_io");
        let format = chars(&mut runtime, "e~n");
        let data = no_args(&mut runtime);
        {
            let context = RuntimeContext::new(&mut runtime);
            assert_eq!(
                context.io_device(device),
                Device::Stream(Stream::Output),
                "`standard_io` is `request/3`'s `group_leader()` rewrite"
            );
        }
        let (result, out) = call_capturing(&mut runtime, "format", &[device, format, data]);
        assert_eq!(runtime.atom_name_of(result.expect("format/3")), Some("ok"));
        assert_eq!(out, "e\n");
    }

    /// The device rules of `o_request/2` + `request/3`: the three kernel io atoms by name,
    /// an unregistered atom with no registered process, and a non-atom/non-pid with no
    /// matching clause.
    #[test]
    fn device_classification_matches_io_erl() {
        let mut runtime = runtime();
        let standard_io = atom(&mut runtime, "standard_io");
        let user = atom(&mut runtime, "user");
        let standard_error = atom(&mut runtime, "standard_error");
        let unregistered = atom(&mut runtime, "no_such_io_device");
        let context = RuntimeContext::new(&mut runtime);
        assert_eq!(
            context.io_device(standard_io),
            Device::Stream(Stream::Output)
        );
        assert_eq!(context.io_device(user), Device::Stream(Stream::Output));
        assert_eq!(
            context.io_device(standard_error),
            Device::Stream(Stream::Stderr)
        );
        // An atom no process is registered under: `request/3`'s `{ErrorTag, arguments}`.
        assert_eq!(context.io_device(unregistered), Device::UnknownAtom);
        // Not an atom and not a pid: no `o_request/2` clause matches.
        assert_eq!(context.io_device(make_small(1)), Device::NoClause);
    }

    /// The oracle's answers, surfaced through the dispatch: `io:nl(foo)` is `error:badarg`
    /// and `io:nl(1)` is the bare `function_clause` (probed against OTP 29.1.1).
    #[test]
    fn a_bad_device_raises_the_oracles_reason() {
        let mut runtime = runtime();
        let foo = atom(&mut runtime, "foo");
        let error = call(&mut runtime, "io", "nl", &[foo]).expect_err("nl(foo)");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");

        let error = call(&mut runtime, "io", "nl", &[make_small(1)]).expect_err("nl(1)");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");
    }

    /// `io:write/2` takes the device first, so an unregistered device atom is the oracle's
    /// `badarg` — never a silent redirect to the output stream.
    #[test]
    fn an_unregistered_device_atom_is_badarg() {
        let mut runtime = runtime();
        let device = atom(&mut runtime, "no_such_io_device");
        let text = chars(&mut runtime, "hi");
        let error = call(&mut runtime, "io", "write", &[device, text]).expect_err("write/2");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }
}
