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

//! The device side of a `{get_until,…}` request: the read loop, the read sizes, the
//! device buffer and its carry-over.
//!
//! [`IoDevice`] owns `file_io_server`'s `#state{handle,buf,read_mode,unic}`
//! (`lib/kernel/src/file_io_server.erl:35`) and reproduces the loop
//! `get_chars/5` → `get_chars_empty/6` / `get_chars_notempty/6` → `get_chars_apply/7`
//! exactly, with the continuation application delegated to a
//! [`Continuation`](super::Continuation):
//!
//! * `get_chars/5` — a non-empty device buffer is fed to the continuation **before** any
//!   fresh read, and is cleared in the process (`file_io_server.erl:586-589`);
//! * `get_chars_empty/6` — read [`read_size`] bytes, then apply; `eof` is applied as the
//!   atom `eof` (`:591-610`);
//! * `get_chars_notempty/6` — read and concatenate with the pending incomplete tail; an
//!   `eof` with a **non-empty** tail is the `invalid_unicode` stop (`:611-625`);
//! * `get_chars_apply/7` — decode per read mode and encoding, narrow to the continuation's
//!   charlist, apply, and either loop (`{more,…}`) or answer and stash the leftover
//!   (`{done,…}`) (`:628-691`).

use std::mem;

use beam_common::Result;
use crate::drivers::efile::FileDataMode;

use super::continuation::{Applied, ContData, Continuation, Leftover};
use super::encoding::{self, Decoded, Encoding, Transcoded};
use super::source::ByteSource;

/// The read size of a `binary`-mode device (`?READ_SIZE_BINARY`, `8 KiB`).
pub const READ_SIZE_BINARY: usize = 8 * 1024;

/// The read size of a `list`-mode device (`?READ_SIZE_LIST`, `128`).
pub const READ_SIZE_LIST: usize = 128;

/// The read size for `mode` — `file_io_server`'s `read_size/1`
/// (`lib/kernel/src/file_io_server.erl:815-818`).
pub const fn read_size(mode: FileDataMode) -> usize {
    match mode {
        FileDataMode::Binary => READ_SIZE_BINARY,
        FileDataMode::List => READ_SIZE_LIST,
    }
}

/// The bytes one device read produced — `get_chars_apply/7`'s `Data0`.
enum Raw {
    /// A chunk read from the source (or the stashed device buffer).
    Bytes(Vec<u8>),
    /// End of file.
    Eof,
}

/// The data handed to the continuation's preparation step — `get_chars_apply/7`'s `Data1`.
///
/// It is `Data0` after the read-mode/encoding step: a code-point list in `list` read mode, a
/// binary in the request's output encoding in `binary` read mode, or `eof`.
enum DecodedData {
    /// A binary in the request's **output** encoding.
    Bytes(Vec<u8>),
    /// A list of code points.
    Chars(Vec<u32>),
    /// End of file.
    Eof,
}

/// The `io_lib:get_until/4` middleman state (`S0`) — `start`, then `{IsDataBinary, Cont}`.
enum Mm<K> {
    /// `io_lib`'s `start`: the first application of this request derives
    /// `IsDataBinary = is_binary(Data1)` and starts the user continuation at `[]`.
    Start,
    /// `io_lib`'s `{IsDataBinary, Cont}`: the data-binary flag is **fixed** by the first
    /// application and the user continuation state is carried forward.
    Cont {
        /// Whether the first `Data1` of the request was a binary.
        is_data_binary: bool,
        /// The user continuation state.
        cont: K,
    },
}

/// One io device: a byte source plus the `file_io_server` state the request loop carries.
///
/// # Example
///
/// ```
/// use beam_io::io_device::{Applied, ContData, Continuation, Encoding, IoDevice, SliceSource};
/// use beam_io::FileDataMode;
/// use beam_common::Result;
///
/// /// Reads one line and returns it.
/// struct Line;
/// impl Continuation for Line {
///     type Cont = Vec<u32>;
///     type Reply = Option<Vec<u32>>;
///     fn apply(
///         &mut self,
///         mut cont: Vec<u32>,
///         data: ContData,
///         _encoding: Encoding,
///     ) -> Result<Applied<Vec<u32>, Option<Vec<u32>>>> {
///         let ContData::Chars(chars) = data else {
///             return Ok(Applied::Done { reply: None, leftover: Default::default() });
///         };
///         if let Some(at) = chars.iter().position(|&c| c == u32::from(b'\n')) {
///             cont.extend_from_slice(&chars[..at]);
///             let rest = chars[at + 1..].to_vec();
///             Ok(Applied::Done {
///                 reply: Some(cont),
///                 leftover: beam_io::io_device::Leftover::Chars(rest),
///             })
///         } else {
///             cont.extend_from_slice(&chars);
///             Ok(Applied::More(cont))
///         }
///     }
/// }
///
/// let mut device = IoDevice::new(
///     SliceSource::new(b"hi\nthere\n"),
///     Encoding::Latin1,
///     FileDataMode::Binary,
/// );
/// let reply = device
///     .get_until(&mut Line, Encoding::Latin1)
///     .expect("a line");
/// assert_eq!(reply, Some(vec![u32::from(b'h'), u32::from(b'i')]));
/// assert_eq!(device.buffer(), b"there\n");
/// ```
pub struct IoDevice {
    /// The device's `read`.
    source: Box<dyn ByteSource>,
    /// The device's stream encoding (`State#state.unic`).
    encoding: Encoding,
    /// The read mode (`State#state.read_mode`).
    read_mode: FileDataMode,
    /// The stashed leftover (`State#state.buf`).
    buffer: Vec<u8>,
}

impl IoDevice {
    /// Builds an io device over `source` with the given stream encoding and read mode.
    pub fn new(
        source: impl ByteSource + 'static,
        encoding: Encoding,
        read_mode: FileDataMode,
    ) -> Self {
        Self {
            source: Box::new(source),
            encoding,
            read_mode,
            buffer: Vec::new(),
        }
    }

    /// Pre-loads the stashed device buffer.
    ///
    /// The oracle's device starts with `buf = <<>>`; a non-empty start mirrors a device
    /// whose leftover survived a previous request.
    pub fn with_buffer(mut self, buffer: Vec<u8>) -> Self {
        self.buffer = buffer;
        self
    }

    /// The device's stream encoding (`State#state.unic`).
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Sets the device's stream encoding (`file:setopts/2`'s `encoding` option).
    pub fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = encoding;
    }

    /// The read mode (`State#state.read_mode`).
    pub fn read_mode(&self) -> FileDataMode {
        self.read_mode
    }

    /// Sets the read mode (`file:setopts/2`'s `binary`/`list` option).
    pub fn set_read_mode(&mut self, read_mode: FileDataMode) {
        self.read_mode = read_mode;
    }

    /// The stashed device buffer (`State#state.buf`).
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    /// Replaces the stashed device buffer.
    pub fn set_buffer(&mut self, buffer: Vec<u8>) {
        self.buffer = buffer;
    }

    /// Takes the stashed device buffer, leaving it empty.
    pub fn take_buffer(&mut self) -> Vec<u8> {
        mem::take(&mut self.buffer)
    }

    /// `true` when a leftover is stashed (the next request feeds it before reading).
    pub fn has_stashed_buffer(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// The device's byte source.
    pub fn source_mut(&mut self) -> &mut dyn ByteSource {
        &mut *self.source
    }

    /// Answers a `{get_until, Encoding, _Prompt, Mod, Func, XtraArgs}` request.
    ///
    /// `continuation` is the `Mod:Func` step (see [`Continuation`]); `out_encoding` is the
    /// request's `Enc` field. The reply is the continuation's reply after `io_lib`'s cast.
    ///
    /// # Errors
    ///
    /// * a failing device read ⇒ the source's typed `BeamError::Io`;
    /// * an invalid `unicode` sequence ⇒ the `invalid_unicode` exit the oracle raises;
    /// * a leftover that cannot be converted back to the stream encoding ⇒ `badarg`;
    /// * whatever the continuation itself returned (the user function raising).
    pub fn get_until<C: Continuation>(
        &mut self,
        continuation: &mut C,
        out_encoding: Encoding,
    ) -> Result<C::Reply> {
        // `get_chars/5`: a stashed buffer is fed first — and cleared — before any read.
        if self.buffer.is_empty() {
            self.get_chars_empty(continuation, out_encoding, Mm::Start)
        } else {
            let buffer = mem::take(&mut self.buffer);
            self.get_chars_apply(continuation, out_encoding, Mm::Start, Raw::Bytes(buffer))
        }
    }

    /// `get_chars_empty/6`: read, then apply (an end of file is applied as `eof`).
    fn get_chars_empty<C: Continuation>(
        &mut self,
        continuation: &mut C,
        out_encoding: Encoding,
        mm: Mm<C::Cont>,
    ) -> Result<C::Reply> {
        let size = read_size(self.read_mode);
        match self.source.read_chunk(size)? {
            Some(bytes) => {
                self.get_chars_apply(continuation, out_encoding, mm, Raw::Bytes(bytes))
            }
            None => self.get_chars_apply(continuation, out_encoding, mm, Raw::Eof),
        }
    }

    /// `get_chars_notempty/6`: read and concatenate with the pending incomplete tail.
    fn get_chars_notempty<C: Continuation>(
        &mut self,
        continuation: &mut C,
        out_encoding: Encoding,
        mm: Mm<C::Cont>,
        pending: Vec<u8>,
    ) -> Result<C::Reply> {
        let size = read_size(self.read_mode);
        match self.source.read_chunk(size) {
            Ok(Some(bytes)) => {
                // The oracle threads `State#state{buf=B}` into `get_chars_apply`
                // (`file_io_server.erl:611-615`), so a failure raised *inside* it — the
                // continuation's own `Mod:Func` raising included — is caught with the pending
                // incomplete tail still in the device buffer (`:686-691`). Write it back before
                // the apply: the `Done` arm overwrites it and every `{more,…}` iteration
                // re-sets it, so the success paths are unchanged.
                let mut concatenated = pending.clone();
                self.buffer = pending;
                concatenated.extend_from_slice(&bytes);
                self.get_chars_apply(
                    continuation,
                    out_encoding,
                    mm,
                    Raw::Bytes(concatenated),
                )
            }
            // `eof` with an empty tail is still `eof`; with a non-empty tail the sequence
            // that spans the end of the file is invalid. The oracle keeps the tail in the
            // device buffer on that stop (`State#state{buf=NewBuff}`).
            Ok(None) if pending.is_empty() => {
                self.get_chars_apply(continuation, out_encoding, mm, Raw::Eof)
            }
            Ok(None) => {
                self.buffer = pending;
                Err(encoding::invalid_unicode())
            }
            Err(error) => {
                self.buffer = pending;
                Err(error)
            }
        }
    }

    /// `get_chars_apply/7`: decode, narrow, apply the continuation, loop or answer.
    fn get_chars_apply<C: Continuation>(
        &mut self,
        continuation: &mut C,
        out_encoding: Encoding,
        mm: Mm<C::Cont>,
        data0: Raw,
    ) -> Result<C::Reply> {
        // `file_io_server.erl:628-691`: the `latin1` clause matches only when the request's
        // `Enc` is `latin1` **and** the device's stream encoding (`State#state.unic`) is
        // `latin1`; every other combination — including `Enc = latin1` on a `unicode` device —
        // takes the general clause with `InEnc = State#state.unic`, `OutEnc = Enc`.
        if out_encoding == Encoding::Latin1 && self.encoding == Encoding::Latin1 {
            self.get_chars_apply_latin1(continuation, mm, data0)
        } else {
            self.get_chars_apply_unicode(continuation, out_encoding, mm, data0)
        }
    }

    /// `get_chars_apply/7`, `latin1` clause (`file_io_server.erl:628-641`).
    fn get_chars_apply_latin1<C: Continuation>(
        &mut self,
        continuation: &mut C,
        mm: Mm<C::Cont>,
        data0: Raw,
    ) -> Result<C::Reply> {
        let data1 = match (self.read_mode, data0) {
            (FileDataMode::List, Raw::Bytes(bytes)) => {
                DecodedData::Chars(encoding::bytes_to_code_points(&bytes))
            }
            (_, Raw::Bytes(bytes)) => DecodedData::Bytes(bytes),
            (_, Raw::Eof) => DecodedData::Eof,
        };
        let (data, is_data_binary, cont) = middleman(mm, data1, Encoding::Latin1)?;
        match continuation.apply(cont, data, Encoding::Latin1)? {
            // `{more, S1}` loops through a fresh read — the `latin1` clause never carries an
            // incomplete tail.
            Applied::More(next) => self.get_chars_empty(
                continuation,
                Encoding::Latin1,
                Mm::Cont {
                    is_data_binary,
                    cont: next,
                },
            ),
            Applied::Done { reply, leftover } => {
                let reply = continuation.cast_reply(reply, is_data_binary, Encoding::Latin1)?;
                self.buffer = cast_binary(&leftover)?;
                Ok(reply)
            }
        }
    }

    /// `get_chars_apply/7`, `unicode` clause (`file_io_server.erl:642-691`).
    fn get_chars_apply_unicode<C: Continuation>(
        &mut self,
        continuation: &mut C,
        out_encoding: Encoding,
        mm: Mm<C::Cont>,
        data0: Raw,
    ) -> Result<C::Reply> {
        let in_encoding = self.encoding;
        let (data1, new_buffer) = match (self.read_mode, data0) {
            (FileDataMode::List, Raw::Bytes(bytes)) => {
                match encoding::decode(&bytes, in_encoding) {
                    Decoded::Complete(chars) => (DecodedData::Chars(chars), Vec::new()),
                    Decoded::Incomplete { prefix, tail } => {
                        (DecodedData::Chars(prefix), tail)
                    }
                    // `{error, [], _}` is the `invalid_unicode` exit; a non-empty prefix is
                    // used with the rest.
                    Decoded::Invalid { prefix, rest } => {
                        if prefix.is_empty() {
                            return Err(encoding::invalid_unicode());
                        }
                        (DecodedData::Chars(prefix), rest)
                    }
                }
            }
            (FileDataMode::Binary, Raw::Bytes(bytes)) => {
                match encoding::transcode(&bytes, in_encoding, out_encoding) {
                    Transcoded::Complete(encoded) => (DecodedData::Bytes(encoded), Vec::new()),
                    Transcoded::Incomplete { prefix, tail } => {
                        (DecodedData::Bytes(prefix), tail)
                    }
                    Transcoded::Invalid { prefix, rest } => {
                        if prefix.is_empty() {
                            return Err(encoding::invalid_unicode());
                        }
                        (DecodedData::Bytes(prefix), rest)
                    }
                }
            }
            // `_ -> {Data0, <<>>}`: the end-of-file atom is passed through.
            (_, Raw::Eof) => (DecodedData::Eof, Vec::new()),
        };
        let (data, is_data_binary, cont) = middleman(mm, data1, out_encoding)?;
        match continuation.apply(cont, data, out_encoding)? {
            // `{more, S1}` loops carrying the incomplete tail for concatenation.
            Applied::More(next) => self.get_chars_notempty(
                continuation,
                out_encoding,
                Mm::Cont {
                    is_data_binary,
                    cont: next,
                },
                new_buffer,
            ),
            Applied::Done { reply, leftover } => {
                let reply = continuation.cast_reply(reply, is_data_binary, out_encoding)?;
                // The leftover is converted back to the device's stream encoding and the
                // incomplete tail is appended (`file_io_server.erl:672-680`).
                let mut buffer = leftover_to_stream_encoding(&leftover, out_encoding, in_encoding)?;
                buffer.extend_from_slice(&new_buffer);
                self.buffer = buffer;
                Ok(reply)
            }
        }
    }
}

/// `io_lib:get_until/4`'s preparation — derive `IsDataBinary` once, narrow `Data1` to the
/// charlist the user function is applied to (`lib/stdlib/src/io_lib.erl:1854-1868`).
///
/// `IsDataBinary` is decided from the **first** data of the request and carried by the
/// continuation from then on (`get_until(start, D, …) -> get_until({is_binary(D), []}, …)`),
/// so only an [`Mm::Start`] inspects the data.
fn middleman<K: Default>(
    mm: Mm<K>,
    data1: DecodedData,
    out_encoding: Encoding,
) -> Result<(ContData, bool, K)> {
    let (is_data_binary, cont) = match mm {
        Mm::Start => (matches!(&data1, DecodedData::Bytes(_)), K::default()),
        Mm::Cont {
            is_data_binary,
            cont,
        } => (is_data_binary, cont),
    };
    let data = match data1 {
        DecodedData::Chars(chars) => ContData::Chars(chars),
        DecodedData::Eof => ContData::Eof,
        DecodedData::Bytes(bytes) => {
            if out_encoding == Encoding::Unicode {
                // `unicode:characters_to_list(Data, utf8)`: the loop only narrows a binary to
                // a charlist in `unicode` mode when it just transcoded *to* `unicode`, so the
                // bytes are well-formed UTF-8. A violation of that invariant is reported — it
                // is never silently truncated.
                match encoding::decode(&bytes, Encoding::Unicode) {
                    Decoded::Complete(chars) => ContData::Chars(chars),
                    _ => {
                        return Err(crate::error::unsupported(
                            "io_device: a `unicode` request's narrowed binary is not valid UTF-8",
                        ))
                    }
                }
            } else {
                ContData::Chars(encoding::bytes_to_code_points(&bytes))
            }
        }
    };
    Ok((data, is_data_binary, cont))
}

/// `cast_binary/1` (`file_io_server.erl:805-812`): the `latin1` clause's device buffer.
fn cast_binary(leftover: &Leftover) -> Result<Vec<u8>> {
    match leftover {
        Leftover::Binary(bytes) => Ok(bytes.clone()),
        Leftover::Chars(chars) => {
            let mut bytes = Vec::with_capacity(chars.len());
            for &char in chars {
                let byte = u8::try_from(char).map_err(|_| encoding::bad_leftover())?;
                bytes.push(byte);
            }
            Ok(bytes)
        }
        Leftover::None => Ok(Vec::new()),
    }
}

/// The `unicode` clause's device buffer: the leftover converted back to the device's stream
/// encoding (`file_io_server.erl:673-680`).
fn leftover_to_stream_encoding(
    leftover: &Leftover,
    out_encoding: Encoding,
    in_encoding: Encoding,
) -> Result<Vec<u8>> {
    match leftover {
        // A binary leftover is bytes in the **output** encoding.
        Leftover::Binary(bytes) => match encoding::transcode(bytes, out_encoding, in_encoding) {
            Transcoded::Complete(encoded) => Ok(encoded),
            // The oracle wraps this in `list_to_binary/1`, which rejects the tuple with
            // `badarg`.
            _ => Err(encoding::bad_leftover()),
        },
        // A list leftover is code points (`unicode:characters_to_binary(Buf, unicode, InEnc)`).
        Leftover::Chars(chars) => {
            encoding::encode_code_points(chars, in_encoding).map_err(|_| encoding::bad_leftover())
        }
        Leftover::None => Ok(Vec::new()),
    }
}