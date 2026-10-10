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

//! The `{get_line, Enc, _Prompt}` io-device request — the line reader behind
//! `file:read_line/1` and `io:get_line/2`.
//!
//! `file:read_line/1` sends `io:request(H, {get_line, latin1, ''})`
//! (`lib/kernel/src/file.erl:1695-1696`) and `io:get_line/2` sends
//! `{get_line, unicode, Prompt}` (`lib/stdlib/src/io.erl:393-394`); the legacy
//! `{get_line, Prompt}` 2-tuple form defaults the encoding to `latin1`
//! (`lib/kernel/src/file_io_server.erl:412-413`). Unlike `{get_until, …}` this request runs
//! **no** Erlang code: `file_io_server` answers it with its own `get_line/4` loop
//! (`:469-487`) over `io_lib:collect_line/4` (`lib/stdlib/src/io_lib.erl:1787-1835`), then casts
//! the collected line (`:382-393`).
//!
//! # What lives where
//!
//! Everything *around* the collection is [`beam_io::io_device`]'s: the device state
//! (`handle`/`buf`/`read_mode`/`unic`), the read sizes (`128` bytes for a `list`-mode device,
//! `8 KiB` for a `binary`-mode one), the stashed leftover fed before a fresh read, the
//! end-of-file atom and the `latin1`/`unicode` conversions — the very loop `{get_until, …}`
//! uses. This module supplies the missing half, the
//! [`Continuation`](beam_io::io_device::Continuation) that implements `io_lib:collect_line/4`
//! and the `cast/4` the reply goes through. The device loop is driven with the device's
//! **stream** encoding as the request's output encoding, which is the oracle's `WorkEnc`
//! (`file_io_server.erl:385-388`): for the two byte-oriented encodings `beam_io` models,
//! `WorkEnc` is exactly `State#state.unic`.
//!
//! # The collect rule
//!
//! A line ends at the first `\n`; a single `\r` immediately **before** that `\n` is dropped
//! (`collect_line_list([$\n|T], [$\r|Stack])`, `io_lib.erl:1828-1829`; the binary `N =:= 1`
//! clause is the same rule when the `\r` ends one chunk and the `\n` begins the next,
//! `:1803-1813`) — so a CRLF pair collapses to the LF, while a lone `\r` (not followed by an
//! LF) is kept. A `\r` at the very end of the input is therefore carried over as the device
//! buffer until the next read proves whether an LF follows. Accumulating every chunk's code
//! points and applying that one rule reproduces `collect_line_bin/4`/`collect_line_list/2`
//! on every input: the oracle drops the byte before the LF only when it is that exact
//! chunk's last byte and the LF is the next chunk's first one, which is precisely "the code
//! point immediately before the LF is a CR". With nothing read, the reply is the atom `eof`.
//!
//! # The cast (`cast(Res, Mode, WorkEnc, OutEnc)`, `file_io_server.erl:782-802`)
//!
//! `Mode` is the device read mode, `WorkEnc` the device stream encoding and `OutEnc` the
//! request's `Enc`:
//!
//! | `Mode` | `WorkEnc` | `OutEnc` | reply |
//! |---|---|---|---|
//! | any | — | — | `eof` ⇒ the atom `eof` (`:782-783`) |
//! | `binary` | `latin1` | `latin1` | the code points as bytes (`:784-785`) |
//! | `binary` | `latin1` | `unicode` | those bytes UTF-8 encoded (`:786-792`) |
//! | `binary` | `unicode` | `unicode` | the code points UTF-8 encoded (validated, `:786-792`) |
//! | `binary` | `unicode` | `latin1` | the code points as bytes, or `no_translation` (`:786-792`) |
//! | `list` | `latin1` | *ignored* | the code points as a char list (`:793-794`) |
//! | `list` | `unicode` | `unicode` | the code points as a char list (`:795-802`) |
//! | `list` | `unicode` | `latin1` | the code points as latin1 bytes, or `no_translation` (`:795-802`) |
//!
//! A code point that does not fit the target encoding makes the oracle's
//! `unicode:characters_to_binary/3`/`characters_to_list/2` fail, which `io_request/2`'s
//! `catch exit:ExError` turns into the reply **value** `{error, {no_translation, InEncoding,
//! OutEncoding}}` **and kills the device process** (`:391-392` + `server_loop/1`) — the
//! `retire` flag [`serve`] returns.
//!
//! # Ground truth
//!
//! * `lib/kernel/src/file_io_server.erl` — `io_request({get_line,…})` (`:382-393`), the
//!   legacy 2-tuple default (`:412-413`), `get_line/4` (`:469-487`), `convert_enc/3`
//!   (`:489-501`), `cast/4` (`:782-802`) and `cast_binary/1` (`:805-812`).
//! * `lib/stdlib/src/io_lib.erl:1787-1835` — `collect_line/4`, `collect_line_bin/4` and
//!   `collect_line_list/2`.
//! * `lib/kernel/src/file.erl:1695-1701` and `lib/stdlib/src/io.erl:393-394` — the two
//!   callers, and the `{ok, Data}` wrapping `file:read_line/1` applies to a list/binary.
//!
//! # Deviations (documented, never a fabricated reply)
//!
//! * **A request encoding beyond `latin1`/`unicode` is a typed gap.** The oracle serves every
//!   `unicode`-module encoding (`utf8`, `utf16`/`utf32` tuples, …); [`Encoding`] models only
//!   the two byte-oriented tags, so anything else — including a non-atom `Enc` — is the typed
//!   `unsupported` capability gap rather than a guessed conversion.
//! * **Device stream encodings** are the same two: `file_io_server`'s `State#state.unic` for a
//!   `{utf16,_}`/`{utf32,_}` mode list is read as `latin1` by
//!   [`Runtime::open_encoding`](super::Runtime::open_encoding) (a divergence shared with
//!   `{get_until, …}` and `{get_chars, …}`).
//! * **An invalid UTF-8 sequence on a `unicode` device** surfaces the device loop's
//!   `invalid_unicode` stop, where the oracle answers `{error, {no_translation, unicode,
//!   latin1}}` (pinned empirically). The bytes cannot be decoded into code points at all, so
//!   no line is fabricated — the typed failure is reported.
//! * **After the `no_translation` retire** a further request reaches a removed device and gets
//!   this crate's "closed io device" typed gap, where the oracle's caller answers
//!   `{error, terminated}` (its `io:request/3` sees the device's `'DOWN'`).

use beam_common::Result;
use beam_io::io_device::{Applied, ContData, Continuation, Encoding, IoDevice, Leftover};
use beam_io::FileDataMode;
use beam_process::ProcessId;
use beam_term::prelude::*;

use crate::error;
use crate::runtime::Runtime;

/// Serves one `{get_line, …}` io-device request, returning `(reply, retire)`.
///
/// `enc` is the request's `Enc` field (`None` for the legacy 2-tuple form, which defaults to
/// `latin1`), and `prompt` is ignored — only the *shell* prints a prompt, exactly as
/// `file_io_server` ignores it.
///
/// The reply is the collected line cast per the module's table — a char list or a binary — or
/// the atom `eof`. `retire` is `true` only for the `no_translation` reply, which in the oracle
/// is a `{stop, …}` that replies and then kills the device process.
///
/// # Errors
///
/// A request this runtime cannot serve faithfully is the typed capability gap documented in
/// the module header — never a fabricated reply.
pub(super) fn serve(
    runtime: &mut Runtime,
    service: ProcessId,
    enc: Option<Eterm>,
    _prompt: Eterm,
) -> Result<(Eterm, bool)> {
    let request_encoding = match enc {
        Some(term) => runtime
            .atom_name(term)
            .and_then(|name| Encoding::from_name(&name))
            .ok_or_else(|| {
                error::unsupported(
                    "file_server: a `{get_line,…}` request encoding that is neither `latin1` nor `unicode`",
                )
            })?,
        // `io_request({get_line,_Prompt}, State) -> io_request({get_line,latin1,_Prompt}, State)`
        // (`file_io_server.erl:412-413`).
        None => Encoding::Latin1,
    };
    let (stream_encoding, read_mode, source, buffer) = {
        let Some(device) = runtime.file_services.devices.get_mut(&service) else {
            return Err(error::unsupported(
                "file_server: a request for a closed io device",
            ));
        };
        let source = device.file.try_clone().map_err(super::io_error)?;
        let mode = if device.binary {
            FileDataMode::Binary
        } else {
            FileDataMode::List
        };
        (
            device.encoding,
            mode,
            source,
            std::mem::take(&mut device.buf),
        )
    };
    let mut io = IoDevice::new(source, stream_encoding, read_mode).with_buffer(buffer);
    // `WorkEnc` (`file_io_server.erl:385-388`): for the byte-oriented encodings `beam_io`
    // models this is exactly the device's stream encoding, so the loop is driven with it.
    let outcome = io.get_until(&mut LineContinuation, stream_encoding);
    // Whatever the loop left stashed is the device's buffer for the next request — the
    // oracle's `State#state.buf`.
    if let Some(device) = runtime.file_services.devices.get_mut(&service) {
        device.buf = io.take_buffer();
    }
    let line = outcome?;
    match cast(runtime, &line, read_mode, stream_encoding, request_encoding)? {
        Casted::Reply(reply) => Ok((reply, false)),
        // `catch exit:ExError -> {stop, ExError, {error, ExError}, State0#state{buf= <<>>}}`
        // (`file_io_server.erl:391-392`): the value `{error, {no_translation, InEnc, OutEnc}}`
        // is replied and the device then dies.
        Casted::NoTranslation => Ok((
            no_translation(runtime, stream_encoding, request_encoding)?,
            true,
        )),
    }
}

/// The oracle's `exit({no_translation, InEncoding, OutEncoding})` reply value
/// (`file_io_server.erl:391-392`, `cast/4`'s `:791`/`:801`).
fn no_translation(runtime: &mut Runtime, stream: Encoding, out: Encoding) -> Result<Eterm> {
    let error_atom = runtime.atom("error")?;
    let tag = runtime.atom("no_translation")?;
    let in_atom = runtime.atom(stream.name())?;
    let out_atom = runtime.atom(out.name())?;
    let reason = runtime.tuple(&[tag, in_atom, out_atom])?;
    runtime.tuple(&[error_atom, reason])
}

/// What `cast/4` produced: the reply term, or the `no_translation` exit the caller boxes.
enum Casted {
    /// The reply term.
    Reply(Eterm),
    /// A code point that does not fit the request's encoding — `cast/4`'s `exit({no_translation,
    /// InEncoding, OutEncoding})`.
    NoTranslation,
}

/// `cast(Res, Mode, WorkEnc, OutEnc)` (`file_io_server.erl:782-802`).
///
/// See the module header for the full table; the reply is built with [`Runtime::binary`]
/// (a binary on the long-lived arena, mirroring `{get_until, …}`) or [`Runtime::list`].
fn cast(
    runtime: &mut Runtime,
    line: &LineReply,
    mode: FileDataMode,
    stream: Encoding,
    out: Encoding,
) -> Result<Casted> {
    // `cast(eof, _, _, _) -> eof` (`:782-783`).
    let LineReply::Line(points) = line else {
        let eof = runtime.atom("eof")?;
        return Ok(Casted::Reply(eof));
    };
    match mode {
        FileDataMode::Binary => {
            let bytes = match (stream, out) {
                // `cast(B, binary, latin1, latin1) -> B` (`:784-785`): the code points of a
                // `latin1` device *are* its bytes.
                (Encoding::Latin1, Encoding::Latin1) => latin1_bytes(points),
                // `cast(B, binary, InEncoding, OutEncoding)` (`:786-792`): a
                // `unicode:characters_to_binary/3` round-trip. `(latin1, unicode)` encodes the
                // latin1 bytes as UTF-8, `(unicode, unicode)` re-validates already-valid UTF-8,
                // and `(unicode, latin1)` drops out when a code point does not fit a byte.
                (Encoding::Latin1, Encoding::Unicode) | (Encoding::Unicode, Encoding::Unicode) => {
                    utf8_bytes(points)
                }
                (Encoding::Unicode, Encoding::Latin1) => latin1_bytes(points),
            };
            let Some(bytes) = bytes else {
                return Ok(Casted::NoTranslation);
            };
            Ok(Casted::Reply(runtime.binary(&bytes)?))
        }
        FileDataMode::List => {
            // `cast(B, list, latin1, _) -> binary_to_list(B)` (`:793-794`): the request's
            // encoding is **ignored** for a `latin1` stream. `cast(B, list, InEncoding,
            // OutEncoding)` (`:795-802`) narrows a `unicode` stream to the request's encoding
            // first.
            let points = match (stream, out) {
                (Encoding::Latin1, _) | (Encoding::Unicode, Encoding::Unicode) => points.clone(),
                (Encoding::Unicode, Encoding::Latin1) => {
                    let Some(bytes) = latin1_bytes(points) else {
                        return Ok(Casted::NoTranslation);
                    };
                    bytes.into_iter().map(u32::from).collect()
                }
            };
            let elements: Vec<Eterm> = points
                .iter()
                .map(|code| make_small(i64::from(*code)))
                .collect();
            Ok(Casted::Reply(runtime.list(&elements)?))
        }
    }
}

/// The code points as `latin1` bytes, or `None` for one that does not fit a byte — the
/// `{error, Prefix, Rest}` of `unicode:characters_to_binary/3` (and of `characters_to_list/2`)
/// the oracle answers `{no_translation, …}` to.
fn latin1_bytes(points: &[u32]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(points.len());
    for &code in points {
        bytes.push(u8::try_from(code).ok()?);
    }
    Some(bytes)
}

/// The code points as UTF-8, or `None` for one that is not a Unicode scalar value (a lone
/// surrogate or above `U+10FFFF`) — the same failure the oracle's
/// `unicode:characters_to_binary/3` reports.
fn utf8_bytes(points: &[u32]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(points.len());
    for &code in points {
        let character = char::from_u32(code)?;
        let mut buffer = [0u8; 4];
        bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
    }
    Some(bytes)
}

/// The line a `{get_line, …}` request collected — `io_lib:collect_line/4`'s `Result`.
#[derive(Clone, PartialEq, Eq, Debug)]
enum LineReply {
    /// The line, including its terminating `\n`, as code points (the `\r` of a CRLF already
    /// dropped).
    Line(Vec<u32>),
    /// The atom `eof` — nothing was left to read.
    Eof,
}

/// `io_lib:collect_line/4` (`lib/stdlib/src/io_lib.erl:1787-1835`) as a
/// [`Continuation`](beam_io::io_device::Continuation) — see the module header for the rule.
///
/// The continuation state is `None` for the oracle's `start` and `Some(accumulated)` once a
/// chunk has been collected without an end of line (`collect_line_list/2`'s `Stack`). Every
/// chunk `beam_io` hands it is a charlist of code points — the raw bytes for a `latin1`
/// device, the decoded code points for a `unicode` one — which is exactly the view
/// `collect_line_bin/4`/`collect_line_list/2` have of their input for the LF/CR search.
struct LineContinuation;

impl Continuation for LineContinuation {
    type Cont = Option<Vec<u32>>;
    type Reply = LineReply;

    fn apply(
        &mut self,
        cont: Self::Cont,
        data: ContData,
        _encoding: Encoding,
    ) -> Result<Applied<Self::Cont, Self::Reply>> {
        let chunks = match data {
            // `collect_line(start, eof, _, _) -> {stop,eof,eof}` and
            // `collect_line(Stack, eof, _, _) -> {stop,reverse(Stack),eof}`
            // (`io_lib.erl:1791-1800`): a stack that collected nothing is still `eof`.
            ContData::Eof => {
                let reply = match cont {
                    Some(points) if !points.is_empty() => LineReply::Line(points),
                    _ => LineReply::Eof,
                };
                return Ok(Applied::Done {
                    reply,
                    leftover: Leftover::None,
                });
            }
            ContData::Chars(chars) => chars,
        };
        let mut accumulated = cont.unwrap_or_default();
        accumulated.extend_from_slice(&chunks);
        let Some(at) = accumulated
            .iter()
            .position(|&code| code == u32::from(b'\n'))
        else {
            // No end of line yet: `collect_line_*` returns the stack and the loop reads on.
            return Ok(Applied::More(Some(accumulated)));
        };
        let mut line = accumulated[..=at].to_vec();
        // A single CR immediately before the LF is dropped — `collect_line_list([$\n|T],
        // [$\r|Stack])` (`io_lib.erl:1828-1829`), and the binary `N =:= 1` clause for a CR that
        // ends one chunk and an LF that begins the next (`:1803-1813`).
        if line.len() >= 2 && line[line.len() - 2] == u32::from(b'\r') {
            line.remove(line.len() - 2);
        }
        // The unconsumed tail is the device buffer for the next request — the oracle's
        // `Cont0`/`cast_binary/1`.
        let rest = accumulated[at + 1..].to_vec();
        Ok(Applied::Done {
            reply: LineReply::Line(line),
            leftover: if rest.is_empty() {
                Leftover::None
            } else {
                Leftover::Chars(rest)
            },
        })
    }
}

#[cfg(test)]
mod tests;
