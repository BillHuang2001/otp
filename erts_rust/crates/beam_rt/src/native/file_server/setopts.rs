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

//! The `{setopts, …}`/`getopts` read-mode surface of the native `file_server_2` io-devices.
//!
//! The device's read mode (`list`/`binary`) and stream encoding are **mutable state**
//! (`file_io_server`'s `State#state.read_mode`/`State#state.unic`): `io:getopts/1` sends the
//! bare atom `getopts` and `io:setopts/2` sends `{setopts, Opts}`
//! (`file_io_server.erl:395-401`). `getopts` answers the live
//! `[{binary, ReadMode =:= binary}, {encoding, Unic}]` (`:752-755`); `setopts` runs the three
//! list transforms of `setopts/2` (`:718-722`), rejects an element that is not a
//! `{binary,_}`/`{encoding,_}` pair with the **value** `{error, enotsup}` (`:729-736`), and
//! applies the surviving encoding/read mode in `do_setopts/2` (`:737-750`), answering `ok` —
//! or throws the `valid_enc/1`-rejected `{error, badarg}` (`:159-178`). Both arms read and
//! write the same `Device` fields the `{get_chars, …}`/`{get_line, …}`/`{get_until, …}`
//! readers use, so a mode change is visible to the next read exactly as it is on the oracle.
//!
//! The two request arms (`("getopts", 0)` and `("setopts", 1)`) live in the parent's
//! `serve_device_request`; everything they are built from lives here.
//!
//! # Honest capability gaps (never a fabricated term)
//!
//! * A `{setopts, …}` encoding `valid_enc/1` accepts but [`beam_io::io_device::Encoding`] has
//!   no counterpart for (`utf16`/`utf32`, a 2/4-byte stream) is the typed gap, never a
//!   fabricated `ok` (the *open-time* equivalent keeps its documented `latin1` divergence).

use beam_common::Result;
use beam_engine::terms;
use beam_io::io_device::Encoding;
use beam_process::ProcessId;
use beam_term::prelude::*;

use super::tuple_fields;
use crate::error;
use crate::runtime::Runtime;

/// The outcome of `file_io_server:valid_enc/1` (`file_io_server.erl:159-178`) for a
/// `{setopts, …}` encoding value, split by what this runtime can do with the accepted
/// result.
#[derive(Debug)]
pub(super) enum SetOptsEncoding {
    /// A byte-oriented encoding [`beam_io::io_device::Encoding`] models, so the device can
    /// honour it: the encoding update is applied.
    Modelled(Encoding),
    /// `valid_enc/1` accepts it, but it is a 2/4-byte stream (`utf16`/`utf32`) this runtime
    /// has no counterpart for — the typed capability gap, never a fabricated `ok`.
    Unmodelled,
    /// `valid_enc/1` rejects it: the caller's `{error, badarg}` reply.
    Invalid,
}

impl Runtime {
    /// `getopts` → `[{binary, ReadMode =:= binary}, {encoding, Unic}]`
    /// (`file_io_server.erl:752-755`).
    ///
    /// The reply is read from the device's **live** read mode and stream encoding, so a
    /// preceding `{setopts, …}` is reflected here — exactly as the oracle's
    /// `State#state.read_mode`/`State#state.unic` are.
    pub(super) fn reply_getopts(&mut self, service: ProcessId) -> Result<(Eterm, bool)> {
        let (binary, encoding) = {
            let device = self.device_mut(service)?;
            (device.binary, device.encoding.name())
        };
        let key = self.atom("binary")?;
        let value = self.atom(if binary { "true" } else { "false" })?;
        let binary_pair = self.tuple(&[key, value])?;
        let key = self.atom("encoding")?;
        let value = self.atom(encoding)?;
        let encoding_pair = self.tuple(&[key, value])?;
        let reply = self.list(&[binary_pair, encoding_pair])?;
        Ok((reply, false))
    }

    /// `{setopts, Opts}` → `ok | {error, badarg} | {error, enotsup}`
    /// (`file_io_server.erl:395-397`, `:718-750`).
    ///
    /// `Opts` is first run through the three list transforms of `setopts/2` (`:718-722`) and
    /// then checked by `check_valid_opts/1` (`:729-736`); a non-2-tuple element or one whose
    /// head is neither `binary` nor `encoding` is the **value** `{error, enotsup}`. The
    /// surviving list is handed to `do_setopts/2` (`:737-750`): the `encoding` value is
    /// classified by `valid_enc/1` (`:159-178`) and the `binary` value selects the read mode
    /// (`true` → binary, `false` → list, absent or the atom `undefined` → unchanged), after
    /// which the request is answered `ok`.
    ///
    /// The two non-`ok` failure shapes are the oracle's: a `valid_enc/1`-rejected encoding is
    /// the **value** `{error, badarg}`, and a non-list `Opts` never reaches `setopts/2` at all
    /// — it is the `io_request/2` catch-all's `{error, {request, {setopts, Opts}}}` reply
    /// (`:418-421`). A `valid_enc/1`-accepted but unmodelled encoding (`utf16`/`utf32`, a
    /// 2/4-byte stream [`beam_io::io_device::Encoding`] has no counterpart for) is the typed
    /// capability gap, never a fabricated `ok` (see the module docs).
    pub(super) fn reply_setopts(&mut self, service: ProcessId, opts: Eterm) -> Result<(Eterm, bool)> {
        let Some(elements) = terms::list_elements(opts).ok() else {
            let request = self.atom("request")?;
            let command = self.atom("setopts")?;
            let pair = self.tuple(&[command, opts])?;
            let reason = self.tuple(&[request, pair])?;
            let reply = self.error_of(reason)?;
            return Ok((reply, false));
        };
        let opts = self.setopts_normalize(&elements)?;
        if !self.check_valid_opts(&opts) {
            return Ok((self.error_tuple("enotsup")?, false));
        }
        // `do_setopts/2`: `valid_enc(proplists:get_value(encoding, Opts, State#state.unic))`
        // runs first — the current encoding is the default, and it is always one of the two
        // modelled ones — and only then is the read mode consulted.
        let current = self.device_mut(service)?.encoding;
        let encoding = match self.proplists_get(&opts, "encoding") {
            Some(value) => match self.classify_setopts_encoding(value) {
                SetOptsEncoding::Modelled(encoding) => encoding,
                SetOptsEncoding::Unmodelled => {
                    return Err(error::unsupported(
                        "file_server: a `{setopts,…}` `utf16`/`utf32` encoding is not supported",
                    ))
                }
                SetOptsEncoding::Invalid => return Ok((self.error_tuple("badarg")?, false)),
            },
            None => current,
        };
        let binary = match self.proplists_get(&opts, "binary") {
            None => None,
            Some(value) => match self.atom_name(value).as_deref() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                // The `undefined` clause of `do_setopts/2` leaves the read mode alone; the
                // oracle reaches it from an absent entry and from an explicit
                // `{binary, undefined}` alike.
                Some("undefined") => None,
                // Any other value has no clause in the oracle's `case` (`:740-747`) and
                // crashes the device there; a typed gap is never a wrong answer.
                _ => {
                    return Err(error::unsupported(
                        "file_server: a non-boolean `{setopts,…}` `binary` value",
                    ))
                }
            },
        };
        let device = self.device_mut(service)?;
        device.encoding = encoding;
        if let Some(binary) = binary {
            device.binary = binary;
        }
        let ok = self.atom("ok")?;
        Ok((ok, false))
    }

    /// The `Opts` list of a `{setopts, Opts}` request after the three list transforms of
    /// `file_io_server:setopts/2` (`file_io_server.erl:718-722`): `expand_encoding/1`
    /// (`:150-157`) → `proplists:substitute_negations([{list,binary}], _)`
    /// (`proplists.erl:438-459`) → `proplists:unfold/1` (`:108-115`).
    ///
    /// A bare `latin1`/`unicode` atom becomes `{encoding, …}`, a `list` property becomes its
    /// `binary` negation, and every remaining bare atom becomes `{Atom, true}`.
    fn setopts_normalize(&mut self, elements: &[Eterm]) -> Result<Vec<Eterm>> {
        let mut out = Vec::with_capacity(elements.len());
        for element in elements {
            // `expand_encoding/1`.
            let expanded = if matches!(self.atom_name(*element).as_deref(), Some("latin1" | "unicode"))
            {
                let key = self.atom("encoding")?;
                self.tuple(&[key, *element])?
            } else {
                *element
            };
            // `substitute_negations([{list,binary}], _)`: the atom `list` and every `list`
            // property (a `{list,true}` included) become `{binary, false}`; `{list,false}`
            // and any other `{list,_}` become `{binary, true}` (the substitution IS the
            // negation, `proplists.erl:438-459`).
            let substituted = match tuple_fields(expanded) {
                Some(fields)
                    if !fields.is_empty()
                        && self.atom_name(fields[0]).as_deref() == Some("list") =>
                {
                    let negated =
                        !(fields.len() == 2 && self.atom_name(fields[1]).as_deref() == Some("true"));
                    self.binary_pair(negated)?
                }
                None if self.atom_name(expanded).as_deref() == Some("list") => {
                    self.binary_pair(false)?
                }
                _ => expanded,
            };
            // `proplists:unfold/1`: a bare atom becomes `{Atom, true}`.
            let unfolded = if is_atom(substituted) {
                let value = self.atom("true")?;
                self.tuple(&[substituted, value])?
            } else {
                substituted
            };
            out.push(unfolded);
        }
        Ok(out)
    }

    /// `file_io_server:check_valid_opts/1` (`file_io_server.erl:729-736`): every element must
    /// be a 2-tuple whose head is the atom `binary` or `encoding`.
    fn check_valid_opts(&self, opts: &[Eterm]) -> bool {
        opts.iter().all(|element| {
            tuple_fields(*element).is_some_and(|fields| {
                fields.len() == 2
                    && matches!(
                        self.atom_name(fields[0]).as_deref(),
                        Some("binary" | "encoding")
                    )
            })
        })
    }

    /// `proplists:get_value/2` (`proplists.erl:222-236`) over a **normalized** `Opts` list
    /// (every element of which `check_valid_opts/1` has already proven to be a 2-tuple with a
    /// `binary`/`encoding` head): the value of the first matching entry, or `None` for the
    /// oracle's `undefined` default.
    fn proplists_get(&self, opts: &[Eterm], key: &str) -> Option<Eterm> {
        for element in opts {
            let Some(fields) = tuple_fields(*element) else {
                continue;
            };
            if fields.len() == 2 && self.atom_name(fields[0]).as_deref() == Some(key) {
                return Some(fields[1]);
            }
        }
        None
    }

    /// `file_io_server:valid_enc/1` (`file_io_server.erl:159-178`) for a `{setopts, …}`
    /// encoding value, split by what this runtime can do with the accepted result.
    fn classify_setopts_encoding(&self, encoding: Eterm) -> SetOptsEncoding {
        match self.atom_name(encoding).as_deref() {
            Some("latin1") => SetOptsEncoding::Modelled(Encoding::Latin1),
            Some("utf8" | "unicode") => SetOptsEncoding::Modelled(Encoding::Unicode),
            // `valid_enc/1` accepts these, but they are 2/4-byte streams with no
            // counterpart in `beam_io::io_device::Encoding`.
            Some("utf16" | "utf32") => SetOptsEncoding::Unmodelled,
            _ => {
                let Some(fields) = tuple_fields(encoding) else {
                    return SetOptsEncoding::Invalid;
                };
                if fields.len() == 2
                    && matches!(self.atom_name(fields[0]).as_deref(), Some("utf16" | "utf32"))
                    && matches!(self.atom_name(fields[1]).as_deref(), Some("big" | "little"))
                {
                    SetOptsEncoding::Unmodelled
                } else {
                    SetOptsEncoding::Invalid
                }
            }
        }
    }

    /// The `{binary, Bool}` pair `proplists:property(binary, Bool)`
    /// (`proplists.erl:91-94`) builds.
    fn binary_pair(&mut self, value: bool) -> Result<Eterm> {
        let key = self.atom("binary")?;
        let value = self.atom(if value { "true" } else { "false" })?;
        self.tuple(&[key, value])
    }
}
