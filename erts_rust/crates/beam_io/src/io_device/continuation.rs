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

//! The continuation seam — the one step of a `{get_until,…}` request that runs **Erlang
//! code**.
//!
//! ## Why this is a seam
//!
//! The request `{get_until, Enc, _Prompt, Mod, Func, XtraArgs}` is answered by
//! `file_io_server:get_chars(io_lib, get_until, {Mod, Func, XtraArgs}, Enc, State)`,
//! whose loop ends in
//!
//! ```erlang
//! apply(Mod, Func, [Cont, Chars | XtraArgs])
//! ```
//!
//! (`lib/stdlib/src/io_lib.erl:1868`). For the `io` module's own calls that is
//! `erl_scan:tokens/4` — **real Erlang code** the device layer cannot run: it needs the
//! engine (`beam_engine`/`beam_rt`), and fabricating its reply is forbidden (workspace §3).
//!
//! So the application is modelled as a caller-supplied [`Continuation`]: the device loop
//! in [`super::IoDevice::get_until`] reproduces everything *around* it — the reads, the
//! encodings, the chunk sizes, the buffer carry-over — and calls the implementor exactly
//! where Erlang calls `Mod:Func`.
//!
//! ## What the continuation is handed
//!
//! `io_lib:get_until/4` narrows the device data to a **charlist of code points** before
//! applying the user function (`is_binary(Data)` → `unicode:characters_to_list(Data, utf8)`
//! or `binary_to_list(Data)`); an end-of-file is passed straight through as the atom
//! `eof`. That narrowing is done by [`super::IoDevice`] (it is pure data conversion), so an
//! implementor sees exactly what `Mod:Func` sees in the oracle: [`ContData::Chars`] or
//! [`ContData::Eof`].
//!
//! ## What the continuation returns
//!
//! `{more, Cont}` or `{done, Result, Buf}` — [`Applied::More`] / [`Applied::Done`]. The
//! `Buf` is the unconsumed tail the device stashes as its buffer (converted back to the
//! device's stream encoding by [`super::IoDevice`]).
//!
//! `io_lib` also **casts** the reply: when the first data of the request was a binary and
//! the user function returned a list, that list becomes a binary
//! (`io_lib.erl:1869-1887`). Because the reply is a caller-defined term, the cast is a
//! [defaulted](Continuation::cast_reply) trait method.

use beam_common::Result;

use super::encoding::Encoding;

/// The data one continuation application is handed — `io_lib`'s `Chars`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ContData {
    /// A charlist of Unicode code points. For a `latin1`-tagged request these are the raw
    /// bytes; for a `unicode`-tagged one they are decoded UTF-8 code points.
    Chars(Vec<u32>),
    /// The atom `eof` — end of file. (`io_lib` passes the atom through unchanged; it is not
    /// a charlist.)
    Eof,
}

impl ContData {
    /// The code points of this data, or `None` for [`ContData::Eof`].
    pub fn as_chars(&self) -> Option<&[u32]> {
        match self {
            ContData::Chars(chars) => Some(chars),
            ContData::Eof => None,
        }
    }

    /// `true` when this is the end-of-file marker.
    pub fn is_eof(&self) -> bool {
        matches!(self, ContData::Eof)
    }
}

/// The unconsumed tail a `{done, Result, Buf}` carries.
///
/// The three shapes are distinct in the oracle: a **binary** `Buf` is bytes *in the
/// request's output encoding*, a **list** `Buf` is code points, and anything else (the atom
/// `eof`, `[]`, `<<>>`) contributes nothing. [`IoDevice::get_until`](super::IoDevice::get_until)
/// converts the first two back into the device's stream encoding and stashes them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Leftover {
    /// No leftover (`<<>>`, `[]`, `eof`, …).
    #[default]
    None,
    /// Bytes in the request's **output** encoding.
    Binary(Vec<u8>),
    /// Code points (a charlist).
    Chars(Vec<u32>),
}

/// What one [`Continuation::apply`] returned — the oracle's `{more, Cont}` / `{done, …}`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Applied<Cont, Reply> {
    /// `{more, NewCont}` — more input is needed; the new continuation state is carried into
    /// the next application.
    More(Cont),
    /// `{done, Result, Buf}` — the request is answered with `reply`, and `leftover` is
    /// stashed as the device buffer.
    Done {
        /// The reply the user function produced (before the `io_lib` cast).
        reply: Reply,
        /// The unconsumed tail the device stashes.
        leftover: Leftover,
    },
}

/// The Erlang function `Mod:Func` of a `{get_until, Enc, _Prompt, Mod, Func, XtraArgs}`
/// request — the one step of the device loop that must run on the engine.
///
/// An implementor owns `XtraArgs` (the extra arguments of the request) and the module /
/// function identities; the device layer supplies the continuation state and the data.
///
/// # Errors
///
/// Returning `Err` models the user function **raising**: `get_chars_apply/7` wraps the
/// application in `catch` and turns `{'EXIT', Reason}` into `{stop, Reason, {error, Func},
/// State}` (`file_io_server.erl:681-682`). The device loop propagates the error unchanged
/// — the `{error, Func}` reply boxing is the io-server protocol's job, and the runtime
/// (which knows `Mod`/`Func`) applies it.
pub trait Continuation {
    /// The continuation state threaded between applications — `erl_scan:tokens/4`'s opaque
    /// continuation. It starts as the empty list, hence [`Default`].
    type Cont: Default;

    /// The reply the user function produces (the `Result` of `{done, Result, Buf}`).
    type Reply;

    /// One application of the continuation to the next chunk of decoded data.
    ///
    /// `encoding` is the request's **output** encoding (`{get_until, Enc, …}`'s `Enc`), so
    /// an implementor that must build a reply can render it as the protocol expects.
    fn apply(
        &mut self,
        cont: Self::Cont,
        data: ContData,
        encoding: Encoding,
    ) -> Result<Applied<Self::Cont, Self::Reply>>;

    /// `io_lib:get_until/4`'s reply cast.
    ///
    /// When the **first** data of the request was a binary (`is_data_binary`) and the user
    /// function returned a list, `io_lib` converts it: to UTF-8 by
    /// `unicode:characters_to_binary(Result, unicode, unicode)` under `unicode`, to bytes by
    /// `erlang:iolist_to_binary(Result)` otherwise (`io_lib.erl:1869-1887`). A reply that is
    /// not a list passes through unchanged — which is why the default implementation is the
    /// identity: the protocol's real continuations (`erl_scan:tokens/4`) return tuples like
    /// `{ok, Tokens, EndLocation}` and never a bare list.
    fn cast_reply(
        &self,
        reply: Self::Reply,
        is_data_binary: bool,
        encoding: Encoding,
    ) -> Result<Self::Reply> {
        let _ = (is_data_binary, encoding);
        Ok(reply)
    }
}

/// The reason [`NoEngineContinuation`] reports.
///
/// It names what is missing precisely: the request loop, the reads, the chunk sizes, the
/// buffer carry-over and the `latin1`/`unicode` conversions all live in this crate, but the
/// application of `Mod:Func` — `erl_scan:tokens/4` for the `io` module's calls — is Erlang
/// code that only an engine can run.
pub const NO_ENGINE_CONTINUATION: &str = concat!(
    "{get_until,…}: no continuation installed — the request loop, the reads and the ",
    "latin1/unicode conversions are implemented, but applying `Mod:Func` (the real ",
    "`erl_scan:tokens/4` for the `io` module) is Erlang code the runtime must supply"
);

/// The [`Continuation`] to use when no engine-backed continuation has been installed.
///
/// Every [`apply`](Continuation::apply) returns the **typed** `unsupported` raise — never a
/// fabricated reply, and never a silent default (workspace §3). A runtime that has not yet
/// adopted the seam gets exactly the capability gap it has today, with a message that says
/// what to install.
///
/// It is also the honest answer for a request whose `Mod:Func` is not implemented:
/// `beam_rt` maps its `no engine` state onto this type.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEngineContinuation;

impl Continuation for NoEngineContinuation {
    type Cont = ();
    type Reply = ();

    fn apply(
        &mut self,
        _cont: (),
        _data: ContData,
        _encoding: Encoding,
    ) -> Result<Applied<(), ()>> {
        Err(crate::error::unsupported(NO_ENGINE_CONTINUATION))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_no_engine_seam_is_the_typed_unsupported() {
        let mut cont = NoEngineContinuation;
        let error = cont
            .apply((), ContData::Eof, Encoding::Unicode)
            .expect_err("no engine installed");
        assert!(error.is_raise(), "an explicit raise, not a fabricated reply");
        let raise = error.into_raise().expect("a raise");
        let reason = raise
            .reason
            .as_atom()
            .expect("an atom reason")
            .to_string();
        assert!(
            reason.starts_with("unsupported: "),
            "the workspace `unsupported:` prefix: {reason}"
        );
        assert!(
            reason.contains("erl_scan:tokens/4"),
            "the reason names what is missing: {reason}"
        );
    }

    #[test]
    fn the_default_reply_cast_is_the_identity() {
        let cont = NoEngineContinuation;
        assert!(cont.cast_reply((), true, Encoding::Unicode).is_ok());
        assert!(cont.cast_reply((), false, Encoding::Latin1).is_ok());
    }

    #[test]
    fn cont_data_distinguishes_a_charlist_from_eof() {
        let chars = ContData::Chars(vec![0x68, 0x69]);
        assert_eq!(chars.as_chars(), Some(&[0x68u32, 0x69][..]));
        assert!(!chars.is_eof());
        assert_eq!(ContData::Eof.as_chars(), None);
        assert!(ContData::Eof.is_eof());
    }
}
