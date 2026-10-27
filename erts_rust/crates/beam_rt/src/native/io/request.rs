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

//! The **stream** io devices: `standard_error` and this runtime's output stream (the
//! terminal the `user` name stands for).
//!
//! Both are the `{io_request, From, ReplyAs, Request}` protocol's *server* side
//! ([`serve`]), and both are reached two ways: a scheduled process's `io.erl` body posts a
//! request to the device (the leader route, [`super::super::super::context::io_route`]), and
//! the floor's `io:request/1,2` / `io:put_chars/2` arms serve the same request without a
//! message ([`super`]).
//!
//! # The vocabularies (probed against OTP 29.1.2)
//!
//! | Request | `standard_error` (`standard_error.erl`) | `user`/`standard_io` (the prim_tty group) |
//! |---|---|---|
//! | `{put_chars, Chars}` / `{put_chars, Enc, Chars}` | `ok` (code points above its limit are escaped, never `no_translation`) | `ok` |
//! | `{put_chars, Mod, F, A}` / `{put_chars, Enc, Mod, F, A}` | `ok` (the payload's `{error, Func}` when it is not chardata) | `ok` |
//! | `getopts` | `[{encoding,unicode},{onlcr,false},{log,none},{terminal,false}]` | the ten-item prim_tty list |
//! | `{setopts, Opts}` | `ok` / `{error, enotsup}` | `ok` |
//! | `{get_geometry, _}` | `{error, enotsup}` | the atom `error` |
//! | `{get_line, …}` / `{get_chars, …}` | unknown → `{error, {request, R}}` | `eof` (`-noinput`) |
//! | `{requests, Rs}` | the last reply; the first `{error, _}` ends the run | same |
//! | anything else | `{error, {request, R}}` (single-wrapped) | `{error, request}` |
//!
//! # Honest residuals
//!
//! * `{get_until, …}` on the **output** stream is a typed capability gap: the oracle answers
//!   `{eof, Pos}` by running the caller's continuation over an end-of-file read, and this
//!   runtime's stream has no position to report. (`standard_error` has no such clause at all,
//!   so there it is the ordinary unknown-request answer.)
//! * The oracle's `user` device process **crashes** on an error inside `{requests, […]}` (a
//!   `group` bug: every later request answers `{error, terminated}`). This runtime has no
//!   `user` process to kill, so the error reply is returned and the device stays usable.
//! * `{setopts, …}` on the output stream accepts the encoding/binary options and answers `ok`
//!   for them; the option *semantics* (the `binary` read mode) have nothing to act on here.

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_io::io_device::Encoding;
use beam_term::prelude::*;

use super::convert::{characters_to_binary, encoding_of, Chars};
use super::{RuntimeContext, Stream};

/// Serves one `{io_request, …}` request on `stream`, answering its **reply value**.
///
/// `None` means "no clause for this request on this device class": the caller shapes that
/// device's own unknown-request answer (`standard_error`'s `{error, {request, R}}` versus the
/// terminal's `{error, request}`).
pub(super) fn serve(
    ctx: &mut RuntimeContext<'_>,
    stream: Stream,
    request: Eterm,
) -> Result<Option<Eterm>> {
    if let Some((command, args)) = command_of(ctx, request) {
        match (command.as_str(), args.len()) {
            // -- character output (the whole reason this module exists) -------------------
            ("put_chars", 1) => return put_chars(ctx, stream, Encoding::Latin1, args[0]).map(Some),
            ("put_chars", 2) => {
                let Some(encoding) = request_encoding(ctx, args[0]) else {
                    return put_chars_bad_encoding(ctx, stream, args[0], args[1]).map(Some);
                };
                return put_chars(ctx, stream, encoding, args[1]).map(Some);
            }
            ("put_chars", 3) => {
                let payload = apply(ctx, args[0], args[1], args[2])?;
                let Some(payload) = payload else {
                    let func = args[1];
                    return applied_bad_payload(ctx, stream, func).map(Some);
                };
                return put_chars(ctx, stream, Encoding::Latin1, payload).map(Some);
            }
            ("put_chars", 4) => {
                let Some(encoding) = request_encoding(ctx, args[0]) else {
                    return put_chars_bad_encoding(ctx, stream, args[0], args[1]).map(Some);
                };
                let payload = apply(ctx, args[1], args[2], args[3])?;
                let Some(payload) = payload else {
                    let func = args[2];
                    return applied_bad_payload(ctx, stream, func).map(Some);
                };
                return put_chars(ctx, stream, encoding, payload).map(Some);
            }
            // `io.erl`'s own client rewrite turns `format`/`write`/`nl` into `put_chars`
            // before they reach a device, but a hand-built `io:request/2` (or an OTP library
            // that predates the rewrite) can still send them.
            ("format", 2) => {
                let rendered = ctx.call_bif("io_lib", "format", 2, &[args[0], args[1]])?;
                return put_chars(ctx, stream, Encoding::Unicode, rendered).map(Some);
            }
            ("write", 1) => {
                let rendered = write_term(ctx, args[0])?;
                return put_chars(ctx, stream, Encoding::Unicode, rendered).map(Some);
            }
            ("nl", 0) => {
                let newline = ctx.charlist("\n")?;
                return put_chars(ctx, stream, Encoding::Unicode, newline).map(Some);
            }
            // -- the request list ---------------------------------------------------------
            ("requests", 1) => return requests(ctx, stream, args[0]).map(Some),
            // -- options ------------------------------------------------------------------
            ("getopts", 0) => return getopts(ctx, stream).map(Some),
            ("setopts", 1) => return setopts(ctx, stream, args[0]).map(Some),
            ("get_geometry", 1) => {
                return match stream {
                    Stream::Stderr => {
                        let reason = ctx.atom("enotsup")?;
                        ctx.error_of(reason).map(Some)
                    }
                    Stream::Output => {
                        let error = ctx.atom("error")?;
                        Ok(Some(error))
                    }
                }
            }
            // -- input under `-noinput` ----------------------------------------------------
            ("get_line", _) | ("get_chars", _) => {
                if stream == Stream::Stderr {
                    return Ok(None);
                }
                let eof = ctx.atom("eof")?;
                return Ok(Some(eof));
            }
            ("get_until", _) => {
                if stream == Stream::Stderr {
                    return Ok(None);
                }
                return Err(crate::error::unsupported(
                    "io: `{get_until, …}` on the output stream (the oracle answers `{eof, Pos}`)",
                ));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// The unknown-request reply of `stream` — `standard_error`'s single-wrapped
/// `{error, {request, R}}` versus the terminal device's `{error, request}`.
pub(super) fn unknown_request(
    ctx: &mut RuntimeContext<'_>,
    stream: Stream,
    request: Eterm,
) -> Result<Eterm> {
    match stream {
        Stream::Stderr => {
            let inner = request_tuple(ctx, request)?;
            ctx.error_of(inner)
        }
        Stream::Output => {
            let reason = ctx.atom("request")?;
            ctx.error_of(reason)
        }
    }
}

/// `{error, Reason}`-shaped reply value.
fn error_reply(ctx: &mut RuntimeContext<'_>, reason: Eterm) -> Result<Eterm> {
    ctx.error_of(reason)
}

/// The `(command, arguments)` pair of a request, or `None` for a request that is not a tuple
/// with an atom head (the protocol's bare-atom `getopts`/`nl` forms are handled as one-word
/// commands).
fn command_of(ctx: &RuntimeContext<'_>, request: Eterm) -> Option<(String, Vec<Eterm>)> {
    if is_atom(request) {
        let name = ctx.atom_name(request)?;
        return Some((name, Vec::new()));
    }
    let fields = crate::native::tuple_fields(request).ok()?;
    let command = ctx.atom_name(fields[0])?;
    Some((command, fields[1..].to_vec()))
}

/// The [`Encoding`] a request names, or `None` when the name is not one `characters_to_binary`
/// knows (which the oracle answers by **raising** inside the device).
fn request_encoding(ctx: &RuntimeContext<'_>, term: Eterm) -> Option<Encoding> {
    encoding_of(ctx.atom_name(term).as_deref())
}

/// Writes the character data `chars` in `from` on `stream`, answering `ok`.
fn put_chars(
    ctx: &mut RuntimeContext<'_>,
    stream: Stream,
    from: Encoding,
    chars: Eterm,
) -> Result<Eterm> {
    match characters_to_binary(chars, from, Encoding::Unicode) {
        Chars::Bytes(bytes) => {
            ctx.write_utf8(stream, &bytes)?;
            ctx.atom("ok")
        }
        // `standard_error` escapes a code point above its limit and never reports
        // `no_translation`; the terminal device's `putc_request` has no such answer, and both
        // report a payload that is not chardata as `{error, put_chars}`.
        Chars::NoTranslation | Chars::BadArg => {
            let reason = ctx.atom("put_chars")?;
            error_reply(ctx, reason)
        }
    }
}

/// The reply for a `{put_chars, Enc, …}` request whose encoding name is unknown: the oracle's
/// device raises `badarg` inside `characters_to_binary/3`, which `standard_error` reports as
/// `{error, put_chars}`.
fn put_chars_bad_encoding(
    ctx: &mut RuntimeContext<'_>,
    _stream: Stream,
    _name: Eterm,
    _payload: Eterm,
) -> Result<Eterm> {
    let reason = ctx.atom("put_chars")?;
    error_reply(ctx, reason)
}

/// The reply for a `{put_chars, Enc, Mod, Func, Args}` request whose payload is not chardata:
/// `file_io_server.erl:366-374` answers `{error, {error, Func}}` (the **function**, not the
/// module), which `io.erl`'s `conv_reason/1` turns into `badarg`.
fn applied_bad_payload(ctx: &mut RuntimeContext<'_>, _stream: Stream, func: Eterm) -> Result<Eterm> {
    let inner = ctx.tuple(&[ctx.atom("error")?, func])?;
    error_reply(ctx, inner)
}

/// Applies `Mod:Func(Args)` for a `{put_chars, …, Mod, Func, Args}` request, answering `None`
/// when the payload is not chardata (a non-list/non-binary result).
fn apply(
    ctx: &mut RuntimeContext<'_>,
    module: Eterm,
    function: Eterm,
    args: Eterm,
) -> Result<Option<Eterm>> {
    let (Some(module), Some(function)) = (ctx.atom_name(module), ctx.atom_name(function)) else {
        return Ok(None);
    };
    let arguments = terms::list_elements(args).map_err(|_| BeamError::BadArg)?;
    let arity = u32::try_from(arguments.len()).map_err(|_| BeamError::SystemLimit)?;
    let rendered = ctx.call_bif(&module, &function, arity, &arguments)?;
    if terms::binary_bytes(rendered).is_ok() || is_list(rendered) {
        return Ok(Some(rendered));
    }
    Ok(None)
}

/// `io_lib:write(Term)` — the `~w` control (see [`super`]'s module docs).
fn write_term(ctx: &mut RuntimeContext<'_>, term: Eterm) -> Result<Eterm> {
    let control = ctx.charlist("~w")?;
    let data = terms::make_list(ctx, &[term])?;
    ctx.call_bif("io_lib", "format", 2, &[control, data])
}

/// `{requests, Requests}`: run each in order; the first `{error, _}` reply ends the run, and
/// the **last** reply is the answer (`file_io_server`'s `io_request_loop/2`).
fn requests(
    ctx: &mut RuntimeContext<'_>,
    stream: Stream,
    list: Eterm,
) -> Result<Eterm> {
    let items = terms::list_elements(list).map_err(|_| BeamError::BadArg)?;
    let mut last = ctx.atom("ok")?;
    for item in items {
        let reply = match serve(ctx, stream, item)? {
            Some(reply) => reply,
            None => unknown_request(ctx, stream, item)?,
        };
        if is_error_tuple(ctx, reply) {
            return Ok(reply);
        }
        last = reply;
    }
    Ok(last)
}

/// Whether `term` is `{error, _}`.
fn is_error_tuple(ctx: &RuntimeContext<'_>, term: Eterm) -> bool {
    match crate::native::tuple_fields(term) {
        Ok(fields) => fields.len() == 2 && ctx.atom_name(fields[0]).as_deref() == Some("error"),
        Err(_) => false,
    }
}

/// `{error, Reason}` — the shared shape of every served failure reply.
fn request_tuple(ctx: &mut RuntimeContext<'_>, request: Eterm) -> Result<Eterm> {
    let request_atom = ctx.atom("request")?;
    ctx.tuple(&[request_atom, request])
}

/// `getopts` — the device's own option list.
fn getopts(ctx: &mut RuntimeContext<'_>, stream: Stream) -> Result<Eterm> {
    // `standard_error:getopts/0` (`lib/kernel/src/standard_error.erl:253-258`) and the
    // `prim_tty` group's ten-item list under `-noinput` (both measured).
    let options: &[(&str, &str)] = match stream {
        Stream::Stderr => &[
            ("encoding", "unicode"),
            ("onlcr", "false"),
            ("log", "none"),
            ("terminal", "false"),
        ],
        Stream::Output => &[
            ("expand_fun", "false"),
            ("echo", "false"),
            ("line_history", "false"),
            ("log", "none"),
            ("binary", "false"),
            ("encoding", "unicode"),
            ("terminal", "false"),
            ("stdout", "false"),
            ("stderr", "false"),
            ("stdin", "false"),
        ],
    };
    let mut items = Vec::with_capacity(options.len());
    for (name, value) in options {
        let key = ctx.atom(name)?;
        let value = ctx.atom(value)?;
        let pair = ctx.tuple(&[key, value])?;
        items.push(pair);
    }
    terms::make_list(ctx, &items)
}

/// `{setopts, Options}` — `ok` for the options this runtime models, `{error, enotsup}`
/// otherwise (`standard_error.erl:219-231`).
fn setopts(ctx: &mut RuntimeContext<'_>, stream: Stream, options: Eterm) -> Result<Eterm> {
    let Ok(items) = terms::list_elements(options) else {
        let reason = ctx.atom("enotsup")?;
        return error_reply(ctx, reason);
    };
    for item in items {
        let accepted = match crate::native::tuple_fields(item) {
            Ok(fields) if fields.len() == 2 => match (ctx.atom_name(fields[0]).as_deref(), stream) {
                (Some("encoding"), _) => encoding_of(ctx.atom_name(fields[1]).as_deref()).is_some(),
                (Some("binary"), Stream::Output) => true,
                (Some("terminal" | "onlcr" | "log" | "expand_fun" | "echo"), _) => true,
                _ => false,
            },
            _ => false,
        };
        if !accepted {
            let reason = ctx.atom("enotsup")?;
            return error_reply(ctx, reason);
        }
    }
    ctx.atom("ok")
}

