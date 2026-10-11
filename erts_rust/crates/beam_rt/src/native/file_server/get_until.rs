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

//! The `{get_until, Enc, Prompt, Mod, Func, XtraArgs}` io-device request — the incremental
//! reader, and the **one** io-device step that runs Erlang code.
//!
//! `file_io_server` answers the request with
//! `get_chars(io_lib, get_until, {Mod, Func, XtraArgs}, Enc, State)`
//! (`lib/kernel/src/file_io_server.erl:362-364`), whose loop reads chunks and ends in
//!
//! ```erlang
//! apply(Mod, Func, [Cont, Chars | XtraArgs])
//! ```
//!
//! (`lib/stdlib/src/io_lib.erl:1868`). `io`'s own calls send the request with
//! `Mod = erl_scan, Func = tokens, XtraArgs = [Pos0, Options]`
//! (`lib/stdlib/src/io.erl:1421-1422`, `:1471`), so the user function is the **re-entrant
//! Erlang scanner** and the reply is `{done, {ok, Tokens, EndLoc} | …, LeftOver} | {more,
//! Cont}`.
//!
//! # What lives where
//!
//! Everything *around* the application is [`beam_io::io_device`]'s: the device state
//! (`handle`/`buf`/`read_mode`/`unic`), the read sizes, the `{more, …}` loop, the stashed
//! leftover fed before a fresh read, the end-of-file atom and the `latin1`/`unicode`
//! conversions. This module supplies the missing half — the
//! [`Continuation`](beam_io::io_device::Continuation) — by loading `Mod`, building
//! `[Cont, Data | XtraArgs]` and running `Mod:Func/arity` on the **engine**.
//!
//! The continuation's state and reply are [`Eterm`]s: `erl_scan:tokens/4`'s continuation
//! starts as the empty list (`Eterm::default()` is `NIL`) and every reply is a tuple
//! (`{ok, …}`/`{eof, …}`/`{error, …}`), so `io_lib`'s reply cast is the identity.
//!
//! # Ground truth
//!
//! * `lib/kernel/src/file_io_server.erl` — `io_request/2` (`:362-396`), `get_chars/5`
//!   (`:573-575`), `get_chars_apply/7` (`:628-691`) and its `{stop, Reason, {error,
//!   err_func(…)}, State}` raise boxing (`:681-682`), `server_loop/1` (`:195-233`) which
//!   *replies first and then dies* on a `{stop, Reason, Reply, State}`.
//! * `lib/stdlib/src/io_lib.erl:1635-1672` — `get_until/4`, the middleman that fixes
//!   `IsDataBinary`, narrows the data to a charlist and casts the reply.
//! * `lib/stdlib/src/erl_scan.erl:344-354` — `tokens/4`, the continuation's protocol.
//!
//! # Deviations (documented, never a fabricated reply)
//!
//! * **Device stream encoding.** The device's `State#state.unic` is the encoding the
//!   `open` mode list selected ([`super::Runtime::open_encoding`]): `unicode` for
//!   `unicode`/`utf8` (bare or as `{encoding, …}`), `latin1` otherwise — including the
//!   `{utf16,_}`/`{utf32,_}` tuple encodings, which have no counterpart in
//!   [`beam_io::io_device::Encoding`] and are read as `latin1` (a divergence shared with
//!   `{get_chars, …}`). The *request* encoding, by contrast, is decoded strictly: an `Enc`
//!   that is neither `latin1` nor `unicode` is the typed capability gap.
//! * **Device buffer sharing with `get_chars`.** A leftover reported by a continuation is
//!   kept in the device and fed to the next `{get_until, …}` request, exactly as
//!   `State#state.buf` is; the `{get_chars, …}` arm does not consult it yet.
//! * **A blocking continuation is a typed gap.** A user function that suspends in a
//!   `receive` would, in the oracle, simply leave the *device process* blocked until a
//!   message arrives. A native service process hosts no engine-side suspension, so the
//!   request is answered with the typed [`crate::error::unsupported`] gap instead of a
//!   fabricated `{more, …}`. (`io`'s real continuation, `erl_scan:tokens/4`, never blocks.)
//! * **A leftover that is a deep iolist is a typed gap.** `Leftover` is a flat
//!   `binary | charlist | nothing`; `cast_binary/1`'s `list_to_binary/1` would flatten a
//!   deep iolist, which this layer cannot represent, so it is reported rather than
//!   truncated. Every protocol continuation in practice answers a flat list, `eof` or `<<>>`.
//! * **`process_flag`-style prompt handling.** The `Prompt` field is ignored, as the file
//!   server does (only the *shell* prints it).

use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{terms, Machine, RunOutcome};
use beam_io::io_device::{Applied, ContData, Continuation, Encoding, IoDevice, Leftover};
use beam_io::FileDataMode;
use beam_loader::Module;
use beam_process::ProcessId;
use beam_term::prelude::*;

use crate::context::RuntimeContext;
use crate::error;
use crate::runtime::Runtime;

use super::{tuple_fields, Device};

/// Serves one `{get_until, …}` io-device request, returning `(reply, retire)`.
///
/// `enc` is the request's `Enc` field (`None` for the legacy 5-tuple form, which defaults to
/// `latin1`), and `rest` is `[Prompt, Mod, Func, XtraArgs]` — the prompt is ignored, exactly
/// as `file_io_server` ignores it.
///
/// The reply is the continuation's own result — `{ok, Tokens, EndLoc}` / `{eof, EndLoc}` /
/// `{error, …}` for `erl_scan:tokens/4` — with no extra wrapping (the `{io_reply, ReplyAs,
/// Reply}` envelope is [`super`]'s). `retire` is `true` when the user function **raised**:
/// the oracle replies with `{error, Func}` and then kills the device
/// (`file_io_server.erl:681-682` + `server_loop/1`'s `exit(Reason)`), which is what retiring
/// the device here mirrors.
///
/// # Errors
///
/// A request this runtime cannot serve faithfully is the typed capability gap documented in
/// the module header — never a fabricated reply.
pub(super) fn serve(
    runtime: &mut Runtime,
    service: ProcessId,
    enc: Option<Eterm>,
    rest: &[Eterm],
) -> Result<(Eterm, bool)> {
    let request_encoding = match enc {
        Some(term) => runtime
            .atom_name(term)
            .and_then(|name| Encoding::from_name(&name))
            .ok_or_else(|| {
                error::unsupported(
                    "file_server: a `{get_until,…}` request encoding that is neither `latin1` nor `unicode`",
                )
            })?,
        None => Encoding::Latin1,
    };
    // `rest` is `[Prompt, Mod, Func, XtraArgs]`; a non-atom module/function cannot be
    // resolved to an export, which is `apply/3`'s `badarg` — the oracle's raise path.
    let (Some(module_name), Some(function)) =
        (runtime.atom_name(rest[1]), runtime.atom_name(rest[2]))
    else {
        return raise_reply(runtime, rest[2]);
    };
    let Ok(xtra_args) = terms::list_elements(rest[3]) else {
        // `apply(Mod, Func, [Cont, Chars | XtraArgs])` with a non-list tail is `badarg`.
        return raise_reply(runtime, rest[2]);
    };
    let (stream_encoding, read_mode, source, buffer) = {
        let device = device(runtime, service)?;
        let source = device
            .file
            .try_clone()
            .map_err(super::io_error)?;
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
    let (outcome, raised) = {
        let mut continuation = EngineContinuation {
            runtime,
            module: None,
            module_name,
            function,
            arity: 2 + xtra_args.len() as u32,
            xtra_args,
            raised: false,
        };
        let outcome = io.get_until(&mut continuation, request_encoding);
        (outcome, continuation.raised)
    };
    // Whatever the loop left stashed is the device's buffer for the next request — the
    // oracle's `State#state.buf`.
    if let Some(device) = runtime.file_services.devices.get_mut(&service) {
        device.buf = io.take_buffer();
    }
    match outcome {
        Ok(reply) => Ok((reply, false)),
        // `file_io_server`'s `{'EXIT', Reason}` clause: reply `{error, Func}` and die.
        Err(_) if raised => raise_reply(runtime, rest[2]),
        Err(failure) => Err(failure),
    }
}

/// The device `service`, or the typed gap for a request that arrived after it was retired.
///
/// The borrow is mutable because serving a request consumes the stashed buffer
/// (`State#state.buf`) and writes the new leftover back into it.
fn device(runtime: &mut Runtime, service: ProcessId) -> Result<&mut Device> {
    runtime
        .file_services
        .devices
        .get_mut(&service)
        .ok_or_else(|| error::unsupported("file_server: a request for a closed io device"))
}

/// The oracle's `{'EXIT', Reason}` boxing of a continuation that raised
/// (`file_io_server.erl:681-682`): the reply is the value `{error, Func}` and the device
/// then dies — the `retire` flag [`serve`] returns.
///
/// `Func` is `err_func/3`'s answer (`file_io_server.erl:697-698`): the *user* function of the
/// `{get_until, _, _, Mod, Func, _}` request, i.e. the `tokens` of `erl_scan:tokens/4`.
fn raise_reply(runtime: &mut Runtime, func: Eterm) -> Result<(Eterm, bool)> {
    let error_atom = runtime.atom("error")?;
    let func_atom = match func_name(runtime, func) {
        Some(name) => runtime.atom(&name)?,
        // A `Func` that is not an atom cannot be named; `badarg` is what `err_func/3`
        // reports for a request whose function is not an atom at all.
        None => runtime.atom("badarg")?,
    };
    let reply = runtime.tuple(&[error_atom, func_atom])?;
    Ok((reply, true))
}

/// The name of `func` when it is an atom.
fn func_name(runtime: &Runtime, func: Eterm) -> Option<String> {
    runtime.atom_name(func)
}

/// The engine-backed `Mod:Func` of a `{get_until, …}` request.
///
/// One instance serves one request; `apply` is called once per chunk the device hands it.
struct EngineContinuation<'r> {
    /// The runtime the user function runs on (the engine needs `&mut`).
    runtime: &'r mut Runtime,
    /// `Mod`, resolved (and cached) on the first application — resolution can fail
    /// (`undef`), which is a raise of the user function, not a device error.
    module: Option<Arc<Module>>,
    /// `Mod`'s name, for the on-demand load.
    module_name: String,
    /// `Func`'s name.
    function: String,
    /// `2 + length(XtraArgs)` — `apply(Mod, Func, [Cont, Chars | XtraArgs])`'s arity.
    arity: u32,
    /// The request's `XtraArgs` (for `io`'s calls, `[Pos0, Options]`).
    xtra_args: Vec<Eterm>,
    /// Whether an application **raised** — the oracle's `{'EXIT', Reason}`, which
    /// `file_io_server` boxes as `{stop, Reason, {error, Func}, State}` rather than
    /// propagating the reason to the caller (see [`raise_reply`]).
    raised: bool,
}

impl EngineContinuation<'_> {
    /// Resolves `Mod` on the engine, memoized, exactly as `apply/3` resolves it: a module the
    /// code path does not have is the `undef` **raise** of the application.
    fn module(&mut self) -> Result<Arc<Module>> {
        if let Some(module) = &self.module {
            return Ok(Arc::clone(module));
        }
        // The code server resolves the system directories lazily, so an on-demand load of a
        // stdlib module works without an explicit `-pa`.
        let _ = self.runtime.find_module_file(&self.module_name);
        let module = self.runtime.code_server.load(&self.module_name)?;
        self.module = Some(Arc::clone(&module));
        Ok(module)
    }

    /// One `apply(Mod, Func, [Cont, Data | XtraArgs])`.
    ///
    /// The arguments are built on the runtime's pointer-stable arena (the same heap the
    /// engine's `alloc_words` uses), so the terms stay valid for the whole request.
    fn call(&mut self, cont: Eterm, data: ContData) -> Result<Eterm> {
        let module = self.module()?;
        let function = module
            .atom_table
            .lookup(&self.function)
            .ok_or_else(error::undef)?;
        if module.export(function, self.arity).is_none() {
            return Err(error::undef());
        }
        let xtra_args = self.xtra_args.clone();
        let args = {
            let mut ctx = RuntimeContext::new(&mut *self.runtime);
            let data_term = match &data {
                ContData::Eof => ctx.atom("eof")?,
                ContData::Chars(chars) => {
                    let elements: Vec<Eterm> = chars
                        .iter()
                        .map(|code| make_small(i64::from(*code)))
                        .collect();
                    terms::make_list(&mut ctx, &elements)?
                }
            };
            let mut args = Vec::with_capacity(2 + xtra_args.len());
            args.push(cont);
            args.push(data_term);
            args.extend(xtra_args);
            args
        };
        let mut machine = Machine::new();
        let outcome = {
            let mut ctx = RuntimeContext::new(&mut *self.runtime);
            // `enter_module` records the call site a BIF reached from the callee resolves its
            // owner from (the same step `RuntimeContext::run_export` takes).
            ctx.enter_module(&module);
            machine.run_export_outcome(&module, function, self.arity, &args, &mut ctx)
        };
        // Adopt the machine's off-heap payload handles **before** it is dropped and before the
        // `?`: both the returned value and a failure's live reason term may still point at a
        // payload this short-lived machine built (see `Runtime::adopt_escaped_binaries`).
        self.runtime.adopt_escaped_binaries(machine.refc_binaries());
        let outcome =
            outcome.map_err(|failure| error::name_atom_raise(failure, self.runtime.atoms()))?;
        match outcome {
            RunOutcome::Returned(value) => Ok(value),
            // The oracle would leave the *device process* blocked in the user function's
            // `receive` until a message arrives; a native service process cannot suspend
            // mid-request, so this is the typed gap rather than a fabricated `{more, …}`.
            RunOutcome::Wait { .. } => Err(error::unsupported(
                "file_server: the `{get_until,…}` continuation suspended in a `receive`",
            )),
        }
    }
}

impl Continuation for EngineContinuation<'_> {
    type Cont = Eterm;
    type Reply = Eterm;

    /// One application of the user function, decoding `io_lib:get_until/4`'s protocol:
    /// `{more, Cont}` → [`Applied::More`], `{done, Reply, Buf}` → [`Applied::Done`].
    ///
    /// Anything else is what `io_lib:get_until/4` itself turns into a `case_clause` — and
    /// `file_io_server`'s `catch` turns *that* into `{error, Func}` again, so it is reported
    /// as a raise (the [`raised`](EngineContinuation::raised) flag) rather than as a device
    /// failure.
    fn apply(
        &mut self,
        cont: Eterm,
        data: ContData,
        _encoding: Encoding,
    ) -> Result<Applied<Eterm, Eterm>> {
        let value = match self.call(cont, data) {
            Ok(value) => value,
            Err(failure) => {
                self.raised = true;
                return Err(failure);
            }
        };
        match interpret(self.runtime, value)? {
            Some(applied) => Ok(applied),
            None => {
                self.raised = true;
                Err(error::unsupported(
                    "file_server: a `{get_until,…}` continuation that returned neither `{more, Cont}` nor `{done, Result, Buf}`",
                ))
            }
        }
    }
}

/// Decodes the user function's return value.
///
/// `Ok(None)` is `io_lib:get_until/4`'s `case_clause`: a value that is neither
/// `{more, Cont}` nor `{done, Result, Buf}`.
fn interpret(runtime: &Runtime, value: Eterm) -> Result<Option<Applied<Eterm, Eterm>>> {
    let Some(fields) = tuple_fields(value) else {
        return Ok(None);
    };
    // `{}` is a tuple with no head — `io_lib:get_until/4`'s `case` has no clause for it
    // (the `case_clause`), so it must not be indexed.
    let Some(head) = fields.first() else {
        return Ok(None);
    };
    let tag = runtime.atom_name(*head);
    match (tag.as_deref(), fields.len()) {
        (Some("more"), 2) => Ok(Some(Applied::More(fields[1]))),
        (Some("done"), 3) => Ok(Some(Applied::Done {
            reply: fields[1],
            leftover: leftover_from(runtime, fields[2])?,
        })),
        _ => Ok(None),
    }
}

/// The leftover of a `{done, Result, Buf}` — `io_lib:get_until/4`'s `Buf`, which
/// `file_io_server` casts with `cast_binary/1` (`file_io_server.erl:805-812`) and stashes in
/// the device.
///
/// The representable shapes are [`beam_io::io_device::Leftover`]'s three: a binary (bytes in
/// the request's output encoding), a charlist of code points, and nothing (`<<>>`, `[]`,
/// `eof`). A **deep iolist** (`[<<"ab">>,"cd"]`) would be flattened by `list_to_binary/1` in
/// the oracle but has no representation here, so it is reported rather than truncated.
fn leftover_from(runtime: &Runtime, buf: Eterm) -> Result<Leftover> {
    if let Ok(bytes) = terms::binary_bytes(buf) {
        return Ok(if bytes.is_empty() {
            Leftover::None
        } else {
            Leftover::Binary(bytes.to_vec())
        });
    }
    if buf == NIL || runtime.atom_name(buf).as_deref() == Some("eof") {
        return Ok(Leftover::None);
    }
    let Ok(elements) = terms::list_elements(buf) else {
        return Err(unrepresentable_leftover());
    };
    let mut chars = Vec::with_capacity(elements.len());
    for element in elements {
        if !is_small(element) {
            return Err(unrepresentable_leftover());
        }
        let code = signed_value(element);
        let Ok(code) = u32::try_from(code) else {
            return Err(unrepresentable_leftover());
        };
        chars.push(code);
    }
    Ok(if chars.is_empty() {
        Leftover::None
    } else {
        Leftover::Chars(chars)
    })
}

/// The typed gap for a `{done, _, Buf}` whose `Buf` this layer cannot represent.
fn unrepresentable_leftover() -> BeamError {
    error::unsupported(
        "file_server: a `{get_until,…}` leftover that is neither a binary, a char list of code points nor `eof`",
    )
}

#[cfg(test)]
mod tests;
