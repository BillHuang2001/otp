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

//! The io-device request layer — `{get_until, Enc, Prompt, Mod, Func, XtraArgs}`.
//!
//! ## The request
//!
//! An Erlang io device answers `{io_request, From, ReplyAs, Request}` messages. One request
//! form is the *incremental reader*:
//!
//! ```text
//! {get_until, Enc, Prompt, Mod, Func, XtraArgs}
//! ```
//!
//! It is served by `file_io_server:io_request/2`
//! (`lib/kernel/src/file_io_server.erl:375-377`) as
//! `get_chars(io_lib, get_until, {Mod, Func, XtraArgs}, Enc, State)`: the device reads
//! chunks and repeatedly applies `io_lib:get_until/4`, which applies the **user** function
//! `Mod:Func(Cont, Chars, XtraArgs…)` until it answers `{done, Result, Buf}`. `io`'s own
//! calls send `{get_until, unicode, Prompt, erl_scan, tokens, [Pos0, Options]}`
//! (`lib/stdlib/src/io.erl:1423`), so the user function is `erl_scan:tokens/4` — the
//! re-entrant Erlang scanner that reads a whole form across arbitrarily many chunks.
//!
//! The `Prompt` field is ignored by the file server.
//!
//! ## What this crate reproduces
//!
//! Everything except the application of `Mod:Func`:
//!
//! * [`IoDevice`] owns the device state (`handle`, `buf`, `read_mode`, `unic`) and the read
//!   loop — the read sizes, the `{more,…}` loop, the **stashed leftover fed before a fresh
//!   read**, the end-of-file delivered as the atom `eof`, and the `invalid_unicode` stop
//!   for a `unicode` sequence that spans the end of the file.
//! * [`Encoding`] plus the module-private conversions reproduce the `latin1`/`unicode`
//!   decoding, the incomplete-UTF-8-tail carry-over (`NewBuff`) and the reply cast.
//! * [`ByteSource`] is the device's `read`, with a real [`std::fs::File`] implementation and
//!   an in-memory [`SliceSource`] for tests.
//! * [`Continuation`] is the `apply(Mod, Func, [Cont, Chars | XtraArgs])` step. It is the
//!   **one** part that runs real Erlang code, so it is a seam the runtime installs;
//!   [`NoEngineContinuation`] is the honest "not installed" implementation, which raises the
//!   typed `unsupported` instead of fabricating a reply.
//!
//! ## Ground truth
//!
//! * `lib/kernel/src/file_io_server.erl` — `io_request/2`, `get_chars/5`,
//!   `get_chars_empty/6`, `get_chars_notempty/6`, `get_chars_apply/7`, `cast`/`cast_binary`,
//!   `read_size/1`.
//! * `lib/stdlib/src/io_lib.erl` — `get_until/4`, the middleman that fixes `IsDataBinary`,
//!   narrows the data to a code-point charlist and casts the reply.
//! * `lib/stdlib/src/erl_scan.erl` — `tokens/4`, the continuation's protocol.
//!
//! ## Deviations
//!
//! * **Term-free.** The reply and the continuation state are caller-defined types, not
//!   `Eterm`s; the charlist the continuation receives is a `Vec<u32>` of code points.
//! * **The `{stop, Reason, {error, Func}, State}` boxing is not performed.** When the
//!   continuation raises, the loop propagates the error unchanged; the io-server protocol's
//!   reply wrapping needs `Mod`/`Func`, which the runtime owns.
//! * **Only `latin1` and `unicode`** stream/request encodings are modelled — the tuple
//!   encodings (`{utf16,binary}`, …) are refused, not guessed.
//! * **No output-encoding cache and no `erl_scan` internals**: the continuation is opaque
//!   to this layer, so the `erl_scan`-specific `invalid_unicode_error/4` position hack
//!   (`file_io_server.erl:695-707`) is not reproduced.
//! * **[`Leftover`] is a flat `binary | charlist | nothing`.** A `Buf` that is a *deep*
//!   iolist (`[<<>>|List]`, `[<<"ab">>,"cd"]`) is not representable; the oracle's
//!   `cast_binary/1` and `unicode:characters_to_binary/3` accept iolists, this layer does not.
//!   Every protocol continuation in practice returns a flat binary, a flat charlist or
//!   `[]`/`<<>>`/`eof`.

mod continuation;
mod device;
mod encoding;
mod source;

#[cfg(test)]
mod tests;

pub use crate::io_device::continuation::{
    Applied, ContData, Continuation, Leftover, NoEngineContinuation, NO_ENGINE_CONTINUATION,
};
pub use crate::io_device::device::{read_size, IoDevice, READ_SIZE_BINARY, READ_SIZE_LIST};
pub use crate::io_device::encoding::Encoding;
pub use crate::io_device::source::{ByteSource, EmptySource, SliceSource};
