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

//! The runtime-native **`file_server_2` service process** (contract A.9).
//!
//! The last directive of the real `bootstrap/bin/start.boot` that does real work is
//! `{apply,{c,erlangrc,[]}}`, and `c:erlangrc/0` (`lib/stdlib/src/c.erl:1049-1083`) reaches
//! the file system through `file:path_eval/2` → `file:open/2` →
//! `gen_server:call(file_server_2, {open,Name,ModeList}, infinity)`. The service process
//! registered under that name is what makes this directive succeed, so the boot script runs
//! to completion (its final directive is `{progress,started}`). Without that service the call
//! would die with the oracle-shaped
//! `{noproc,{gen_server,call,[file_server_2,…]}}` and the boot would stop there.
//!
//! This module supplies the missing **service process**: a [`beam_process::Process`] that
//! is spawned, registered as `file_server_2` and kept **permanently `Runnable`** — never
//! put on a run queue, never given a `RunnerState` engine, exactly like the runtime's own
//! `init` process (`Runtime::new`). Because `Process::wake` only wakes a `Waiting`/
//! `Suspended` process, a message delivered into its mailbox never enqueues it, so the
//! scheduler's runner never sees it and `error::unknown_process_entry` is unreachable.
//! Its mailbox is drained by [`Runtime::service_file_requests`], an **additive** step of
//! [`Runtime::scheduler_pass`](crate::runtime::Runtime) run right after the run queue
//! drains — the same place the port half runs — which is what lets a reply wake a caller
//! blocked in `gen_server:call`'s `receive`.
//!
//! # The wire protocols (verbatim from `lib/`)
//!
//! * **Request.** `gen.erl:231-250`: for a *local* pid and `infinity` — the only form this
//!   runtime produces — `gen:call/4` does `Mref = erlang:monitor(process,Pid)`,
//!   `Pid ! {'$gen_call', {self(), Mref}, Request}`, then waits for `{Mref, Reply}`. The
//!   **alias** form (`Tag = [alias|Ref]`, `gen.erl:251-281`) is only the remote/timeout
//!   clause and is not reachable here; it is left an honest capability gap rather than
//!   fabricating an alias delivery this runtime cannot perform.
//! * **Reply.** `gen.erl:547-554` `reply/2`: `{To,Tag}` with an alias tag sends
//!   `{Tag,Reply}` to the alias reference, otherwise `To ! {Tag, Reply}`. The reply is
//!   always the 2-tuple `{Tag, Reply}` with `Tag` echoed **unchanged**.
//! * **`file_server` request surface.** `file_server.erl:101-201` `handle_call`: the
//!   command is the head of the `Request` tuple and the arguments its tail.
//! * **The write path.** `{write_file, Name, Bin}` is `?PRIM_FILE:write_file(Name, Bin)`
//!   (`file_server.erl:111-112`, the body of `file:write_file/2`), and a write-mode
//!   `{open, Name, ModeList}` (`[write]`/`[append]`) opens the path **through the floor's
//!   own `prim_file:open/2`** so the `get_flags` derivation, the errno vocabulary and
//!   `efile_open`'s post-open type checks stay in one place. `file:write_file/3`
//!   (`file.erl:1211-1241`) and `file:open/2` + `file:write/2` + `file:close/1` ride those
//!   two arms over the io-device's already-served `put_chars`/`close` requests.
//! * **The io-device.** A successful `{open,Name,ModeList}` returns `{ok, IoPid}`
//!   (`file_io_server:start_link/3`, `file_io_server.erl:57-96`). `IoPid` speaks the
//!   `{file_request,From,ReplyAs,Request}` → `{file_reply,ReplyAs,Reply}` protocol for
//!   `close`/`{position,At}`/`sync`/`truncate` and the
//!   `{io_request,From,ReplyAs,Request}` → `{io_reply,ReplyAs,Reply}` protocol for
//!   `{get_chars,Enc,_Prompt,N}`/`{put_chars,…}` (`file_io_server.erl:199-233`).
//!
//! # The `{get_until, …}` incremental reader
//!
//! `{get_until, Enc, Prompt, Mod, Func, XtraArgs}` is the one request that runs **Erlang
//! code**: `file_io_server` answers it with `get_chars(io_lib, get_until, {Mod,Func,XtraArgs},
//! …)`, whose loop ends in `apply(Mod, Func, [Cont, Chars | XtraArgs])` — for `io`'s own
//! calls `erl_scan:tokens/4`. The device state, the read loop and the `latin1`/`unicode`
//! conversions live in `beam_io`'s [`io_device`](beam_io::io_device) module; this crate
//! supplies the missing half — the [`Continuation`](beam_io::io_device::Continuation) — by
//! running the real export on the engine (see [`get_until`]).
//!
//! # The `{get_line, …}` line reader
//!
//! `{get_line, Enc, _Prompt}` — what `file:read_line/1` and `io:get_line/2` send — runs **no**
//! Erlang code: `file_io_server` collects the line with its own `get_line/4` loop over
//! `io_lib:collect_line/4` and casts it. The device loop is the same one `{get_until, …}`
//! uses; this crate supplies the collect-and-cast half (see [`get_line`]).
//!
//! # Honest capability gaps (never a fabricated term)
//!
//! * `{read_file_info, Name}` and `{read_link_info, Name}` are **partly** served by the
//!   [`file_info`] child: the `stat(2)`/`lstat(2)` failure is the oracle's `{error, Reason}`
//!   verbatim, while a *successful* stat — whose reply would need the `#file_info{}` `access`
//!   slot — stays the typed capability gap (the child's docs carry the full argument).
//! * Every *other* request this module does not serve is reported as the typed
//!   [`crate::error::unsupported`] gap rather than a plausible-looking wrong answer
//!   (`setopts`, `getopts`, `pread`/`pwrite`, `advise`, `allocate`,
//!   `read_handle_info`, the legacy `put_chars` arities, a tuple encoding).
//! * An unknown `file_server` `handle_call` request is the oracle's `{noreply, State}`:
//!   the service replies **nothing** and carries on.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::HeapPolicy;
use beam_io::io_device::Encoding;
use beam_process::{ProcessId, ProcessTable};
use beam_term::prelude::*;

use crate::context::RuntimeContext;
use crate::error;
use crate::native::prim_file::take_open_file;
use crate::runtime::Runtime;

mod file_info;
mod get_line;
mod get_until;

/// The native file service: the `file_server_2` process plus one **io-device** process per
/// successful `open`.
///
/// Both kinds are [`beam_process::Process`]es that live in the runtime's process table,
/// stay permanently `Runnable` and are drained by [`Runtime::service_file_requests`].
#[derive(Debug)]
pub(crate) struct FileServices {
    /// The registered `file_server_2` process.
    server: ProcessId,
    /// The heap policy every io-device is spawned with (cloned from the runtime's own, so a
    /// device's `process_info(Pid, min_heap_size | …)` matches a runtime-spawned process).
    policy: HeapPolicy,
    /// The live io-devices, keyed by their own pid.
    devices: HashMap<ProcessId, Device>,
}

/// One open file, owned by its io-device process.
#[derive(Debug)]
struct Device {
    /// The open OS file. `std::fs::File` is syscall-backed, so it carries the errno the
    /// oracle's `posix_error_to_tuple/1` reports.
    file: std::fs::File,
    /// The read mode `file_io_server:parse_options/1` derived (`binary`/`list`): whether a
    /// read is shaped as a binary or as a char list.
    binary: bool,
    /// The device's own **stream** encoding (`file_io_server`'s `State#state.unic`): `unicode`
    /// when the open's mode list carried `unicode`/`utf8` (or `{encoding,unicode|utf8}`),
    /// `latin1` otherwise — `parse_options/1`'s default, and the value every explicitly
    /// modelled encoding maps to here. The `{utf16,_}`/`{utf32,_}` tuple encodings are *not*
    /// modelled and also read as `latin1` (a documented divergence, unchanged by the
    /// `{get_until,…}` work).
    encoding: Encoding,
    /// The leftover `{get_until, …}` request stashed in the device (`State#state.buf`): the
    /// bytes a continuation reported as unconsumed, fed to the next request before any
    /// fresh read.
    buf: Vec<u8>,
}

impl FileServices {
    /// Spawns the `file_server_2` service process and registers it under that name.
    ///
    /// The process is born `Runnable` and never enqueued (see the module docs), so this is
    /// the runtime-construction step: nothing can run it, and only
    /// [`Runtime::service_file_requests`] ever reads its mailbox.
    ///
    /// # Errors
    ///
    /// The process table's allocation diagnostics, and a typed error when the name is
    /// already taken (the runtime registers it exactly once, at construction).
    pub(crate) fn start(
        processes: &mut ProcessTable,
        atoms: &mut beam_term::AtomTable,
        policy: HeapPolicy,
    ) -> Result<FileServices> {
        let server = processes.spawn(policy.clone())?;
        let name = atoms.intern("file_server_2");
        processes
            .register(name, server)
            .map_err(|_| error::message("boot: the `file_server_2` name is already registered"))?;
        Ok(FileServices {
            server,
            policy,
            devices: HashMap::new(),
        })
    }

    /// The registered `file_server_2` pid.
    pub(crate) const fn server(&self) -> ProcessId {
        self.server
    }
}

impl Runtime {
    /// Drains every native file service's mailbox: the `file_server_2` requests and the
    /// per-device `{file_request,…}`/`{io_request,…}` requests, delivering each reply into
    /// the requesting process's mailbox and returning how many replies were delivered.
    ///
    /// This is the **native-service half** of a scheduler pass, run right after the run
    /// queue drains — the counterpart of
    /// [`Runtime::deliver_port_messages`](crate::port::PortRegistry). A reply is what wakes
    /// a caller blocked in `gen_server:call`'s `receive`, but — and this is the whole point
    /// — the requesting process here is normally the runtime's own `self_pid`, which is
    /// `Runnable` and therefore is **not** moved onto the run queue by the delivery; the
    /// caller resumes through the engine's blocking-aware entry (`RunOutcome::Wait` →
    /// `Machine::resume_outcome`), not through a scheduler slice. Counting the reply into
    /// the pass's `delivered` total is what tells the boot's drive that its blocked call
    /// can make progress — a serviced-only pass must never look like "nothing happened".
    ///
    /// # Errors
    ///
    /// A request this module cannot serve faithfully is a typed capability gap (see the
    /// module docs), never a fabricated reply; the process table's delivery diagnostics,
    /// and its allocation failures while spawning an io-device.
    pub(crate) fn service_file_requests(&mut self, table: &mut ProcessTable) -> Result<usize> {
        // The servers are collected first so `self` is free while `table` is borrowed.
        let mut servers = Vec::with_capacity(1 + self.file_services.devices.len());
        servers.push(self.file_services.server());
        servers.extend(self.file_services.devices.keys().copied());
        let mut delivered = 0;
        for service in servers {
            loop {
                // The mailbox payload has already been copied onto the service's own heap by
                // `deliver_simple`, and a service process never runs and never collects, so
                // the term is stable for as long as it is read here.
                let Some(message) = table
                    .get_mut(service)
                    .and_then(|process| process.receive(|_| true))
                else {
                    break;
                };
                let payload = message.payload();
                let Some((target, envelope)) = self.serve_file_message(table, service, payload)?
                else {
                    continue;
                };
                // A reply to a caller that has since died is dropped, exactly as `gen:reply/2`
                // swallows its own send failure.
                // A reply to a caller that has since died is dropped, exactly as `gen:reply/2`
                // swallows its own send failure. The delivery is the runtime's one
                // **fragment-aware** path (`RuntimeContext::deliver_in_table`), which falls
                // back to a heap fragment on the copier's refusal: a caller that is
                // **relocation-exempt** (a process under the `-eval` evaluator or a nested
                // machine, whose roots a collection cannot publish) refuses the collection an
                // exhausted delivery would otherwise run, so the fragment is what lets its
                // reply land at all.
                match RuntimeContext::deliver_in_table(
                    self,
                    table,
                    target,
                    service.to_eterm(),
                    envelope,
                ) {
                    Ok(_) => delivered += 1,
                    Err(BeamError::TermNotFound) => {}
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(delivered)
    }

    /// Decodes one message for `service` and returns the `(target, reply envelope)` to
    /// deliver, or `None` when the message is not a request this service answers (the
    /// oracle's `_ -> server_loop(State)` and its `{noreply, State}` catch-all).
    ///
    /// `target` is the pid the reply goes to: for a `{'$gen_call', {Pid, Tag}, Request}` it
    /// is `Pid` (the local `infinity` form `gen:reply/2` sends to), and for a
    /// `{file_request, From, …}`/`{io_request, From, …}` it is `From`.
    fn serve_file_message(
        &mut self,
        table: &mut ProcessTable,
        service: ProcessId,
        payload: Eterm,
    ) -> Result<Option<(ProcessId, Eterm)>> {
        let Some(fields) = tuple_fields(payload) else {
            return Ok(None);
        };
        // The io-device protocol: {file_request|io_request, From, ReplyAs, Request}.
        if fields.len() == 4 {
            let tag = self.atom_name(fields[0]);
            // The reply tag is **paired** with the request tag, never echoed:
            // `file_io_server`'s `file_reply/3`/`io_reply/3` answer `{file_request, …}` with
            // `{file_reply, ReplyAs, Reply}` and `{io_request, …}` with
            // `{io_reply, ReplyAs, Reply}` (`file_io_server.erl:199-239`), and every caller
            // matches the reply on its own tag (`io.erl:1670`'s `execute_request`).
            let reply_tag = match tag.as_deref() {
                Some("file_request") => "file_reply",
                Some("io_request") => "io_reply",
                _ => return Ok(None),
            };
            let Ok(from) = ProcessId::from_eterm(fields[1]) else {
                return Ok(None);
            };
            let reply_as = fields[2];
            let (reply, retire) = self.serve_device_request(service, fields[3])?;
            if retire {
                self.file_services.devices.remove(&service);
            }
            let tag_atom = self.atom(reply_tag)?;
            let envelope = self.tuple(&[tag_atom, reply_as, reply])?;
            return Ok(Some((from, envelope)));
        }
        // The gen_server protocol: {'$gen_call', {Pid, Tag}, Request}.
        if fields.len() == 3 && self.atom_name(fields[0]).as_deref() == Some("$gen_call") {
            let Some(from) = tuple_fields(fields[1]) else {
                return Ok(None);
            };
            if from.len() != 2 {
                return Ok(None);
            }
            let Ok(caller) = ProcessId::from_eterm(from[0]) else {
                return Ok(None);
            };
            let tag = from[1];
            // The alias form is a typed gap, never a fabricated delivery (see module docs).
            if let Some(alias) = improper_list_head(tag) {
                if self.atom_name(alias).as_deref() == Some("alias") {
                    return Err(error::unsupported(
                        "file_server: the `[alias|Ref]` reply form is not supported",
                    ));
                }
            }
            let reply = match self.serve_server_request(table, fields[2])? {
                Some(reply) => reply,
                // The oracle's `handle_call(Request, From, State) -> {noreply, State}`.
                None => return Ok(None),
            };
            let envelope = self.tuple(&[tag, reply])?;
            return Ok(Some((caller, envelope)));
        }
        Ok(None)
    }

    /// Serves one `file_server:handle_call/3` request (`file_server.erl:101-201`),
    /// returning the reply **value** — or `None` for the requests the oracle answers
    /// `{noreply, State}` to.
    fn serve_server_request(&mut self, table: &mut ProcessTable, request: Eterm) -> Result<Option<Eterm>> {
        // `get_cwd` is the one command `init` sends as a bare atom as well as a 1-tuple.
        if self.atom_name(request).as_deref() == Some("get_cwd") {
            return self.prim_file_1("get_cwd", &[]).map(Some);
        }
        let Some(fields) = tuple_fields(request) else {
            return Ok(None);
        };
        let Some(command) = self.atom_name(fields[0]) else {
            return Ok(None);
        };
        let args = &fields[1..];
        match (command.as_str(), args.len()) {
            ("open", 2) => self.reply_open(table, args).map(Some),
            ("read_file", 1) => self.prim_file_1("read_file", args).map(Some),
            // `file_server.erl:111-112`: `?PRIM_FILE:write_file(Name, Bin)` — the body of
            // `file:write_file/2` (`file.erl:1193-1194`). `file:write_file/3` reaches the same
            // write through its own open→write→close over this module's `{open, …}` and
            // io-device seams (`file.erl:1211-1241`, `do_write_file/3`).
            ("write_file", 2) => self.prim_file_2("write_file", args).map(Some),
            ("delete", 1) => self.prim_file_1("delete", args).map(Some),
            ("make_dir", 1) => self.prim_file_1("make_dir", args).map(Some),
            ("del_dir", 1) => self.prim_file_1("del_dir", args).map(Some),
            ("read_link", 1) => self.prim_file_1("read_link", args).map(Some),
            ("altname", 1) => self.prim_file_1("altname", args).map(Some),
            ("rename", 2) => self.prim_file_2("rename", args).map(Some),
            ("make_link", 2) => self.prim_file_2("make_link", args).map(Some),
            ("make_symlink", 2) => self.prim_file_2("make_symlink", args).map(Some),
            ("get_cwd", 0) => self.prim_file_1("get_cwd", &[]).map(Some),
            ("get_cwd", 1) => self.prim_file_1("get_cwd", args).map(Some),
            ("list_dir", 1) => self.reply_list_dir(args).map(Some),
            // `file_server.erl:141-145`/`:156-160`: `read_file_info` follows symlinks
            // (`stat(2)`), `read_link_info` does not (`lstat(2)`). Both the 1-arity and the
            // `Opts`-carrying 2-arity form are the same `read_info_1/3`
            // (`file_info.rs` reads `args.get(1)`).
            ("read_file_info", 1 | 2) => file_info::serve(self, true, args).map(Some),
            ("read_link_info", 1 | 2) => file_info::serve(self, false, args).map(Some),
            // Everything else — `copy`, `write_file_info`, … — has no faithful
            // native implementation here and is a typed gap, never a wrong value.
            _ => Err(error::unsupported(&format!(
                "file_server: the `{command}/{}` request is not implemented",
                args.len()
            ))),
        }
    }

    /// `{open, Name, ModeList}` (`file_server.erl:101-104`).
    ///
    /// A `ModeList` that is not a list is the oracle's `{error, einval}`
    /// (`file_server.erl:105-106`). The mode list's **stream encoding** is then validated, and
    /// only then is the open performed: the oracle's `file_io_server:parse_options/1` runs
    /// before its `OpenFun` (`file_io_server.erl:90-92`), so a `valid_enc/1`-rejected encoding
    /// is the caller's `{error, badarg}` and must never reach `open(2)` — an invalid encoding
    /// never creates or truncates the path.
    ///
    /// The open itself is **delegated** to the floor's own `prim_file:open/2`, the one owner
    /// of the `get_flags` derivation (`unix_prim_file.c:142-171`), the
    /// `posix_error_to_tuple/1` errno vocabulary and `efile_open`'s post-open type checks
    /// (`eisdir`/`enotdir`/`exclusive`/`append`/`truncate`, `unix_prim_file.c:196-212`); its
    /// `{error, Reason}` answer is returned verbatim. A `{ok, Descriptor}` has its OS file
    /// **moved out** by [`take_open_file`], so the path is never opened twice and the device
    /// owns the very handle the floor opened — which is what makes a write-mode
    /// (`[write]`/`[append]`) open create a missing path. A readable regular file yields
    /// `{ok, IoPid}` with a fresh native io-device.
    fn reply_open(&mut self, table: &mut ProcessTable, args: &[Eterm]) -> Result<Eterm> {
        if !is_list(args[1]) {
            return self.error_tuple("einval");
        }
        if !self.open_encoding_is_valid(args[1]) {
            return self.error_tuple("badarg");
        }
        let reply = self.prim_file_2("open", args)?;
        let Some(fields) = tuple_fields(reply) else {
            return Err(error::message(
                "file_server: prim_file:open/2 did not answer a tuple",
            ));
        };
        if fields.len() != 2 {
            return Err(error::message(
                "file_server: prim_file:open/2 did not answer a 2-tuple",
            ));
        }
        if self.atom_name(fields[0]).as_deref() != Some("ok") {
            return Ok(reply);
        }
        let Some(file) = take_open_file(self, fields[1]) else {
            return Err(error::message(
                "file_server: prim_file:open/2's descriptor holds no open file",
            ));
        };
        let binary = self.open_is_binary(args[1]);
        let encoding = self.open_encoding(args[1]);
        let device = table.spawn(self.file_services.policy.clone())?;
        self.file_services.devices.insert(
            device,
            Device {
                file,
                binary,
                encoding,
                buf: Vec::new(),
            },
        );
        let ok = self.atom("ok")?;
        self.tuple(&[ok, device.to_eterm()])
    }

    /// Whether `ModeList`'s stream-encoding options survive
    /// `file_io_server:parse_options/1`'s validation (`file_io_server.erl:131-148`).
    ///
    /// `parse_options/1` expands a bare `latin1`/`unicode` atom into `{encoding, …}` first
    /// (`expand_encoding/1`, `:150-157`) and then calls `valid_enc/1` (`:159-178`) on every
    /// `{encoding, E}` it meets; `{error, badarg}` aborts the whole start (`:90-92`, `:107-110`).
    /// Every other mode-list element — an unknown atom like a bare `utf16` included — is an
    /// ordinary option and never reaches `valid_enc`, so only an explicit `{encoding, E}` pair
    /// can be rejected here.
    fn open_encoding_is_valid(&self, mode_list: Eterm) -> bool {
        terms::list_elements(mode_list)
            .map(|elements| {
                elements.iter().all(|element| {
                    let Some(fields) = tuple_fields(*element) else {
                        return true;
                    };
                    fields.len() != 2
                        || self.atom_name(fields[0]).as_deref() != Some("encoding")
                        || self.valid_encoding(fields[1])
                })
            })
            .unwrap_or(true)
    }

    /// `file_io_server:valid_enc/1` (`file_io_server.erl:159-178`): the encodings
    /// `parse_options/1` accepts. Anything else is the caller's `{error, badarg}`.
    fn valid_encoding(&self, encoding: Eterm) -> bool {
        if matches!(
            self.atom_name(encoding).as_deref(),
            Some("latin1" | "utf8" | "unicode" | "utf16" | "utf32")
        ) {
            return true;
        }
        let Some(fields) = tuple_fields(encoding) else {
            return false;
        };
        if fields.len() != 2 {
            return false;
        }
        let kind = self.atom_name(fields[0]);
        matches!(
            (kind.as_deref(), self.atom_name(fields[1]).as_deref()),
            (Some("utf16" | "utf32"), Some("big" | "little"))
        )
    }
    /// Whether `ModeList` selects the `binary` read mode
    /// (`file_io_server:parse_options/1` defaults to `list`).
    fn open_is_binary(&self, mode_list: Eterm) -> bool {
        terms::list_elements(mode_list)
            .map(|elements| {
                elements.iter().any(|element| {
                    is_atom(*element) && self.atom_name(*element).as_deref() == Some("binary")
                })
            })
            .unwrap_or(false)
    }

    /// The read mode `ModeList` selects the **stream encoding** of the device it opens
    /// (`file_io_server:parse_options/1`'s `unicode` result, reached through
    /// `expand_encoding/1`).
    ///
    /// Only the two byte-oriented encodings `beam_io` models are recognised: a `unicode`
    /// (or `utf8`) atom, or an `{encoding, unicode | utf8}` pair, selects
    /// [`Encoding::Unicode`]; every other mode list — including an absent `encoding` option
    /// and the `{utf16,_}`/`{utf32,_}` tuple encodings, which have no counterpart here —
    /// selects [`Encoding::Latin1`], `parse_options/1`'s default.
    fn open_encoding(&self, mode_list: Eterm) -> Encoding {
        let is_unicode = |term: Eterm| {
            matches!(self.atom_name(term).as_deref(), Some("unicode") | Some("utf8"))
        };
        let unicode = terms::list_elements(mode_list)
            .map(|elements| {
                elements.iter().any(|element| {
                    is_unicode(*element)
                        || tuple_fields(*element).is_some_and(|fields| {
                            fields.len() == 2
                                && self.atom_name(fields[0]).as_deref() == Some("encoding")
                                && is_unicode(fields[1])
                        })
                })
            })
            .unwrap_or(false);
        if unicode {
            Encoding::Unicode
        } else {
            Encoding::Latin1
        }
    }

    /// `file_server:handle_call({list_dir, Name}, …)` → `prim_file:list_dir/1`:
    /// `{ok, Names}` with the entries in the **host directory order** (`file.erl:1044`
    /// documents "The names are not sorted"), or `{error, Reason}`.
    fn reply_list_dir(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(path) = self.encode_path(args[0]) else {
            return self.error_tuple("badarg");
        };
        match std::fs::read_dir(OsStr::from_bytes(&path)) {
            Err(failure) => self.error_tuple_for(&failure),
            Ok(entries) => {
                let mut names = Vec::new();
                for entry in entries {
                    match entry {
                        Ok(entry) => names.push(entry.file_name().to_string_lossy().into_owned()),
                        Err(failure) => return self.error_tuple_for(&failure),
                    }
                }
                let mut terms_out = Vec::with_capacity(names.len());
                for name in &names {
                    terms_out.push(self.charlist(name)?);
                }
                let list = self.list(&terms_out)?;
                let ok = self.atom("ok")?;
                self.tuple(&[ok, list])
            }
        }
    }

    /// Serves one io-device request, returning `(reply, retire)`.
    ///
    /// A request this module cannot serve faithfully is the typed capability gap described
    /// in the module docs — never a plausible-looking wrong term.
    fn serve_device_request(&mut self, service: ProcessId, request: Eterm) -> Result<(Eterm, bool)> {
        // `file.erl` sends `close`/`sync`/`datasync`/`truncate` as the **bare** command atom
        // (`close(File) when is_pid(File) -> file_request(File, close)`, `file.erl:1536`) and
        // the positioned forms as a 1-tuple (`{position,At}`, `file.erl:1954`), so a request is
        // either an atom or a tuple whose head is the command.
        let (command, args) = if is_atom(request) {
            match self.atom_name(request) {
                Some(name) => (name, Vec::new()),
                None => return Err(error::unsupported("file_server: an unnamed io request")),
            }
        } else {
            let Some(fields) = tuple_fields(request) else {
                return Err(error::unsupported("file_server: a non-tuple io request"));
            };
            let Some(command) = self.atom_name(fields[0]) else {
                return Err(error::unsupported("file_server: an unnamed io request"));
            };
            (command, fields[1..].to_vec())
        };
        let args = &args[..];
        match (command.as_str(), args.len()) {
            // -- file_request ------------------------------------------------------------
            ("close", 0) => {
                let ok = self.atom("ok")?;
                Ok((ok, true))
            }
            ("sync", 0) => {
                self.device_file(service)?.sync_all().map_err(io_error)?;
                let ok = self.atom("ok")?;
                Ok((ok, false))
            }
            ("datasync", 0) => {
                self.device_file(service)?.sync_data().map_err(io_error)?;
                let ok = self.atom("ok")?;
                Ok((ok, false))
            }
            ("truncate", 0) => {
                let file = self.device_file(service)?;
                let position = file.stream_position().map_err(io_error)?;
                file.set_len(position).map_err(io_error)?;
                let ok = self.atom("ok")?;
                Ok((ok, false))
            }
            ("position", 1) => {
                let seek = seek_from(args[0], self)?;
                let file = self.device_file(service)?;
                let position = file.seek(seek).map_err(io_error)?;
                let ok = self.atom("ok")?;
                let value = make_small(i64::try_from(position).map_err(|_| BeamError::SystemLimit)?);
                let reply = self.tuple(&[ok, value])?;
                Ok((reply, false))
            }
            // -- io_request --------------------------------------------------------------
            ("get_chars", 3) => self.reply_get_chars(service, args),
            ("put_chars", 2) => self.reply_put_chars(service, args),
            // The incremental reader: `{get_until,Enc,_Prompt,Mod,Func,XtraArgs}`
            // (`file_io_server.erl:362-364`) and its legacy 5-tuple form, which defaults the
            // encoding to `latin1` (`:395-396`).
            ("get_until", 5) => get_until::serve(self, service, Some(args[0]), &args[1..]),
            ("get_until", 4) => get_until::serve(self, service, None, args),
            // The line reader: `{get_line,Enc,_Prompt}` (`file_io_server.erl:382-393`) and its
            // legacy 2-tuple form, which defaults the encoding to `latin1` (`:412-413`).
            ("get_line", 2) => get_line::serve(self, service, Some(args[0]), args[1]),
            ("get_line", 1) => get_line::serve(self, service, None, args[0]),
            _ => Err(error::unsupported(&format!(
                "file_server: the `{command}/{}` io request is not implemented",
                args.len()
            ))),
        }
    }

    /// `{get_chars, Enc, _Prompt, N}` → `Data | eof` (`file_io_server.erl:509-575`).
    ///
    /// Only the byte-oriented `latin1` encoding is served: a request in any other encoding
    /// would need `unicode:characters_to_binary/3`, which this native process does not host.
    fn reply_get_chars(&mut self, service: ProcessId, args: &[Eterm]) -> Result<(Eterm, bool)> {
        if self.atom_name(args[0]).as_deref() != Some("latin1") {
            return Err(error::unsupported(
                "file_server: a non-latin1 `get_chars` encoding is not supported",
            ));
        }
        if !is_small(args[2]) {
            return Err(error::unsupported("file_server: a non-integer `get_chars` count"));
        }
        let count = signed_value(args[2]);
        let Ok(count) = usize::try_from(count) else {
            return Err(error::unsupported("file_server: a negative `get_chars` count"));
        };
        let binary = self.device_binary(service)?;
        let mut buffer = vec![0u8; count];
        let read = if count == 0 {
            0
        } else {
            self.device_file(service)?.read(&mut buffer).map_err(io_error)?
        };
        buffer.truncate(read);
        if read == 0 && count != 0 {
            let eof = self.atom("eof")?;
            return Ok((eof, false));
        }
        let data = if binary {
            self.binary(&buffer)?
        } else {
            let bytes: Vec<Eterm> = buffer.iter().map(|byte| make_small(i64::from(*byte))).collect();
            self.list(&bytes)?
        };
        Ok((data, false))
    }

    /// `{put_chars, Enc, Chars}` → `ok` (`file_io_server.erl:420-445`).
    fn reply_put_chars(&mut self, service: ProcessId, args: &[Eterm]) -> Result<(Eterm, bool)> {
        if self.atom_name(args[0]).as_deref() != Some("latin1") {
            return Err(error::unsupported(
                "file_server: a non-latin1 `put_chars` encoding is not supported",
            ));
        }
        let Some(bytes) = iodata_bytes(args[1]) else {
            return Err(error::unsupported("file_server: a non-latin1 `put_chars` payload"));
        };
        self.device_file(service)?.write_all(&bytes).map_err(io_error)?;
        let ok = self.atom("ok")?;
        Ok((ok, false))
    }

    /// The io-device's open file.
    fn device_file(&mut self, service: ProcessId) -> Result<&mut std::fs::File> {
        self.file_services
            .devices
            .get_mut(&service)
            .map(|device| &mut device.file)
            .ok_or_else(|| error::unsupported("file_server: a request for a closed io device"))
    }

    /// The device's read mode (binary vs list).
    fn device_binary(&mut self, service: ProcessId) -> Result<bool> {
        self.file_services
            .devices
            .get(&service)
            .map(|device| device.binary)
            .ok_or_else(|| error::unsupported("file_server: a request for a closed io device"))
    }

    // -- term helpers -------------------------------------------------------------
    //
    // These are the small shims over the shared `RuntimeContext` builders the floor already
    // uses (`crate::native::prim`), so a reply's shape cannot drift from the rest of the
    // host's term construction.

    /// The atom `name`, interned in the runtime's shared atom table.
    fn atom(&mut self, name: &str) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        ctx.atom(name)
    }

    /// `text` as an Erlang string (a char list).
    fn charlist(&mut self, text: &str) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        ctx.charlist(text)
    }

    /// The name of the atom `term`, resolved through the shared atom table.
    fn atom_name(&self, term: Eterm) -> Option<String> {
        if !is_atom(term) {
            return None;
        }
        self.atoms().name(atom_val(term)).map(str::to_string)
    }

    /// A tuple of `elements`.
    fn tuple(&mut self, elements: &[Eterm]) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        terms::make_tuple(&mut ctx, elements)
    }

    /// A proper list of `elements`.
    fn list(&mut self, elements: &[Eterm]) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        terms::make_list(&mut ctx, elements)
    }

    /// A binary built on the runtime's long-lived `-eval` heap (which, unlike
    /// `terms::make_heap_binary`, has no 64-byte limit).
    fn binary(&mut self, bytes: &[u8]) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        beam_codec::term::make_binary(ctx.eval_heap(), bytes)
    }

    /// `{error, Reason}` for a path `prim_file` rejects.
    fn error_tuple(&mut self, reason: &str) -> Result<Eterm> {
        let error = self.atom("error")?;
        let reason = self.atom(reason)?;
        self.tuple(&[error, reason])
    }

    /// `{error, Reason}` for a host I/O failure — the C's
    /// `posix_error_to_tuple(erl_errno_id(errno))`.
    fn error_tuple_for(&mut self, failure: &std::io::Error) -> Result<Eterm> {
        let reason = errno_atom(failure)?;
        self.error_tuple(&reason)
    }

    /// The `prim_file:<function>/<arity>` implementations this runtime already has, reached
    /// through the floor's own dispatch so the shapes cannot drift.
    fn prim_file_1(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        self.prim_file_call(function, args.len() as u32, args)
    }

    /// The 2-argument `prim_file` implementations.
    fn prim_file_2(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        self.prim_file_call(function, 2, args)
    }

    /// Runs one `prim_file` primitive on [`self`] through the floor's dispatcher.
    fn prim_file_call(&mut self, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        match ctx.prim_file_call(function, arity, args) {
            Some(result) => result,
            None => Err(error::unsupported(&format!(
                "file_server: prim_file:{function}/{arity} is not implemented"
            ))),
        }
    }

    /// The path's bytes, or `None` for the path forms `prim_file` rejects
    /// (`{error, badarg}`).
    fn encode_path(&mut self, term: Eterm) -> Option<Vec<u8>> {
        let ctx = RuntimeContext::new(&mut *self);
        ctx.encode_path(term)
    }
}

/// The elements of a tuple term, or `None` when `term` is not a tuple.
fn tuple_fields(term: Eterm) -> Option<Vec<Eterm>> {
    let arity = terms::tuple_arity(term).ok()?;
    let mut fields = Vec::with_capacity(arity);
    for index in 0..arity {
        fields.push(terms::tuple_element(term, index).ok()?);
    }
    Some(fields)
}

/// The head of an **improper** list `[Head | Tail]` whose tail is not a list — the shape of
/// `gen:reply/2`'s `[alias|Ref]` tag. `None` for a proper list (or tail) and for a non-list.
///
/// `is_list/1` is the **proper**-list predicate, so it reports `false` for `[alias|Ref]`; the
/// cons accessors are what decide whether the term is a cons cell at all.
fn improper_list_head(term: Eterm) -> Option<Eterm> {
    let tail = terms::cons_tail(term).ok()?;
    if tail == NIL || is_list(tail) {
        return None;
    }
    terms::cons_head(term).ok()
}

/// The `SeekFrom` a `{position, At}` request denotes (`file_io_server.erl:268-273`).
fn seek_from(at: Eterm, runtime: &Runtime) -> Result<SeekFrom> {
    // A bare integer is the `bof` shorthand of `file.erl`'s `location()`
    // (`integer() | {bof,integer()} | cur | {cur,integer()} | eof | {eof,integer()}`).
    if is_small(at) {
        let offset = signed_value(at);
        return Ok(SeekFrom::Start(
            u64::try_from(offset).map_err(|_| BeamError::BadArg)?,
        ));
    }
    let name = runtime.atom_name(at);
    match name.as_deref() {
        Some("bof") => return Ok(SeekFrom::Start(0)),
        Some("cur") => return Ok(SeekFrom::Current(0)),
        Some("eof") => return Ok(SeekFrom::End(0)),
        _ => {}
    }
    let Some(fields) = tuple_fields(at) else {
        return Err(error::unsupported("file_server: an unknown `position` argument"));
    };
    if fields.len() != 2 || !is_small(fields[1]) {
        return Err(error::unsupported("file_server: an unknown `position` argument"));
    }
    let offset = signed_value(fields[1]);
    match runtime.atom_name(fields[0]).as_deref() {
        Some("bof") => Ok(SeekFrom::Start(u64::try_from(offset).map_err(|_| BeamError::BadArg)?)),
        Some("cur") => Ok(SeekFrom::Current(offset)),
        Some("eof") => Ok(SeekFrom::End(offset)),
        _ => Err(error::unsupported("file_server: an unknown `position` argument")),
    }
}

/// The bytes of an iodata term (a `latin1` char list, a nested list, or a binary).
///
/// `None` for anything else — a code point above 255 has no `latin1` byte, and a nested
/// binary is not a `latin1` payload.
fn iodata_bytes(term: Eterm) -> Option<Vec<u8>> {
    if term == NIL {
        return Some(Vec::new());
    }
    if is_small(term) {
        let code = signed_value(term);
        return u8::try_from(code).ok().map(|byte| vec![byte]);
    }
    if let Ok(bytes) = terms::binary_bytes(term) {
        return Some(bytes.to_vec());
    }
    let elements = terms::list_elements(term).ok()?;
    let mut out = Vec::new();
    for element in elements {
        out.extend(iodata_bytes(element)?);
    }
    Some(out)
}

/// The oracle's errno atom for a host I/O failure.
///
/// `std::fs` is syscall-only, so every failure carries an errno — the same input the C's
/// `posix_error_to_tuple(erl_errno_id(errno))` takes. An errno this runtime has no atom
/// name for is the typed capability gap, never an invented atom.
fn errno_atom(failure: &std::io::Error) -> Result<String> {
    let code = failure.raw_os_error().unwrap_or(0);
    let name = match code {
        libc::EPERM => "eperm",
        libc::ENOENT => "enoent",
        libc::EINTR => "eintr",
        libc::EIO => "eio",
        libc::EBADF => "ebadf",
        libc::EAGAIN => "eagain",
        libc::ENOMEM => "enomem",
        libc::EACCES => "eacces",
        libc::EFAULT => "efault",
        libc::EBUSY => "ebusy",
        libc::EEXIST => "eexist",
        libc::EXDEV => "exdev",
        libc::ENODEV => "enodev",
        libc::ENOTDIR => "enotdir",
        libc::EISDIR => "eisdir",
        libc::EINVAL => "einval",
        libc::ENFILE => "enfile",
        libc::EMFILE => "emfile",
        libc::ENOSPC => "enospc",
        libc::ESPIPE => "espipe",
        libc::EROFS => "erofs",
        libc::EMLINK => "emlink",
        libc::EPIPE => "epipe",
        libc::ENAMETOOLONG => "enametoolong",
        libc::ENOTEMPTY => "enotempty",
        libc::ELOOP => "eloop",
        libc::EOVERFLOW => "eoverflow",
        libc::ENOTSUP => "enotsup",
        libc::ENOSYS => "enosys",
        _ => {
            return Err(error::unsupported(&format!(
                "file_server: no errno atom for error code {code}"
            )))
        }
    };
    Ok(name.to_string())
}

/// Maps a host I/O failure onto the runtime's error type: the oracle never raises for one,
/// so a failure inside a served request is a typed runtime error.
fn io_error(failure: std::io::Error) -> BeamError {
    error::message(format!("file_server: host I/O failed: {failure}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime with the boot floor loaded, so the shared atom table holds the names the
    /// reply shapes need.
    ///
    /// `pub(super)` so the `get_until` child's tests reuse the one runtime fixture rather
    /// than booting a second, subtly different one.
    pub(super) fn runtime() -> Runtime {
        let mut runtime = Runtime::with_defaults().expect("a runtime");
        runtime.boot().expect("boot");
        runtime
    }

    /// Builds a term on `runtime`'s arena through a fresh context.
    pub(super) fn build(
        runtime: &mut Runtime,
        f: impl FnOnce(&mut RuntimeContext) -> Eterm,
    ) -> Eterm {
        let mut ctx = RuntimeContext::new(runtime);
        f(&mut ctx)
    }

    #[test]
    fn the_service_is_registered_under_file_server_2() {
        let runtime = runtime();
        let name = runtime
            .atoms()
            .lookup("file_server_2")
            .expect("the name is interned at construction");
        let pid = runtime
            .processes()
            .whereis(name)
            .expect("the service is registered");
        assert_eq!(pid, runtime.file_services.server());
    }

    /// The `{error, enoent}` value the boot's `file:open("$HOME/.erlang",[read])` receives.
    #[test]
    fn the_error_tuple_shape_is_the_oracles() {
        let mut runtime = runtime();
        let reply = runtime.error_tuple("enoent").expect("a term");
        let fields = tuple_fields(reply).expect("a tuple");
        assert_eq!(runtime.atom_name(fields[0]).as_deref(), Some("error"));
        assert_eq!(runtime.atom_name(fields[1]).as_deref(), Some("enoent"));
    }

    /// `file_io_server:parse_options/1` defaults to `list`; only an explicit `binary` in the
    /// mode list selects the binary read mode.
    #[test]
    fn the_open_mode_selects_the_read_mode() {
        let mut runtime = runtime();
        let list_mode = build(&mut runtime, |ctx| {
            let read = ctx.atom("read").unwrap();
            terms::make_list(ctx, &[read]).unwrap()
        });
        assert!(!runtime.open_is_binary(list_mode));
        let binary_mode = build(&mut runtime, |ctx| {
            let read = ctx.atom("read").unwrap();
            let binary = ctx.atom("binary").unwrap();
            terms::make_list(ctx, &[read, binary]).unwrap()
        });
        assert!(runtime.open_is_binary(binary_mode));
    }

    #[test]
    fn errno_atoms_are_the_oracle_names() {
        let failure = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(errno_atom(&failure).unwrap(), "enoent");
        let failure = std::io::Error::from_raw_os_error(libc::EISDIR);
        assert_eq!(errno_atom(&failure).unwrap(), "eisdir");
        let failure = std::io::Error::from_raw_os_error(libc::ENOTEMPTY);
        assert_eq!(errno_atom(&failure).unwrap(), "enotempty");
        // An errno this runtime has no atom for is the typed gap, never an invented atom.
        let failure = std::io::Error::from_raw_os_error(9999);
        assert!(errno_atom(&failure).is_err());
    }

    #[test]
    fn iodata_bytes_flattens_lists_nested_lists_and_binaries() {
        let mut runtime = runtime();
        let term = build(&mut runtime, |ctx| {
            let one = make_small(65);
            let two = make_small(66);
            let inner = terms::make_list(ctx, &[two]).unwrap();
            let binary = terms::make_heap_binary(ctx, b"CD").unwrap();
            terms::make_list(ctx, &[one, inner, binary]).unwrap()
        });
        assert_eq!(iodata_bytes(term).unwrap(), b"ABCD");
        // A code point above 255 has no latin1 byte, so it is not a latin1 payload.
        assert!(iodata_bytes(make_small(300)).is_none());
    }

    /// The `[alias|Ref]` tag `gen:reply/2` sends to the alias reference is detected as an
    /// improper list; a plain `{Pid, Ref}` tag is not.
    #[test]
    fn the_alias_tag_is_detected_as_an_improper_list() {
        let mut runtime = runtime();
        let alias_tag = build(&mut runtime, |ctx| {
            let alias = ctx.atom("alias").unwrap();
            terms::make_cons(ctx, alias, make_small(0)).unwrap()
        });
        assert!(improper_list_head(alias_tag).is_some());
        let proper_tag = build(&mut runtime, |ctx| {
            let alias = ctx.atom("alias").unwrap();
            let nil_tail = terms::make_list(ctx, &[]).unwrap();
            terms::make_cons(ctx, alias, nil_tail).unwrap()
        });
        assert!(improper_list_head(proper_tag).is_none());
        assert!(improper_list_head(make_small(0)).is_none());
    }

    /// A `{position, bof}` request seeks to the start.
    #[test]
    fn seek_from_reads_the_position_arguments() {
        let mut runtime = runtime();
        let at = build(&mut runtime, |ctx| ctx.atom("bof").unwrap());
        assert!(matches!(seek_from(at, &runtime).unwrap(), SeekFrom::Start(0)));
        let at = build(&mut runtime, |ctx| {
            let cur = ctx.atom("cur").unwrap();
            terms::make_tuple(ctx, &[cur, make_small(7)]).unwrap()
        });
        assert!(matches!(seek_from(at, &runtime).unwrap(), SeekFrom::Current(7)));
        // A bare integer is the `bof` shorthand of `file.erl`'s `location()`.
        assert!(matches!(
            seek_from(make_small(3), &runtime).unwrap(),
            SeekFrom::Start(3)
        ));
        // An unknown form is the typed gap.
        let at = build(&mut runtime, |ctx| ctx.atom("nope").unwrap());
        assert!(seek_from(at, &runtime).is_err());
    }

    #[test]
    fn tuple_fields_rejects_a_non_tuple() {
        assert!(tuple_fields(make_small(1)).is_none());
    }

    // -- the write path (contract A.9) ---------------------------------------------------

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use beam_process::SignalAtoms;

    /// A path in the OS temp directory, unique per test and per run.
    fn unique_path(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "beam_rt_file_server_{label}_{}_{unique}",
            std::process::id()
        ))
    }

    /// A path that is removed when the guard drops.
    struct TempPath(PathBuf);

    impl TempPath {
        /// A unique path with any leftover removed first, so a `[write]` open must create it.
        fn new(label: &str) -> TempPath {
            let path = unique_path(label);
            let _ = std::fs::remove_file(&path);
            TempPath(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A process table for the `serve_*` seams, which take the runtime **and** the table
    /// apart (the shape a scheduler pass has; the runtime's own table is a private field).
    /// The devices map is keyed by pid and only ever read by pid, so a table of its own is
    /// enough to allocate the io-device pids these tests then drive.
    fn table(runtime: &mut Runtime) -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(runtime.code_server.atoms_mut()))
    }

    /// The char-list path term for `path`.
    fn path_term(runtime: &mut Runtime, path: &Path) -> Eterm {
        let text = path.to_str().expect("a UTF-8 temporary path");
        build(runtime, |ctx| ctx.charlist(text).expect("a char list"))
    }

    /// A mode list of atoms.
    fn mode_list(runtime: &mut Runtime, modes: &[&str]) -> Eterm {
        build(runtime, |ctx| {
            let atoms: Vec<Eterm> = modes
                .iter()
                .map(|mode| ctx.atom(mode).expect("an atom"))
                .collect();
            terms::make_list(ctx, &atoms).expect("a list")
        })
    }

    /// `{open, Name, Modes}`.
    fn open_request(runtime: &mut Runtime, name: Eterm, modes: Eterm) -> Eterm {
        build(runtime, |ctx| {
            let open = ctx.atom("open").expect("an atom");
            terms::make_tuple(ctx, &[open, name, modes]).expect("a tuple")
        })
    }

    /// `{write_file, Name, Payload}` with `payload` as a binary.
    fn write_file_request(runtime: &mut Runtime, name: Eterm, payload: &[u8]) -> Eterm {
        build(runtime, |ctx| {
            let write_file = ctx.atom("write_file").expect("an atom");
            let payload = terms::make_heap_binary(ctx, payload).expect("a binary");
            terms::make_tuple(ctx, &[write_file, name, payload]).expect("a tuple")
        })
    }

    /// The `~w` rendering of `term`, without the printer's trailing newline.
    fn render(runtime: &mut Runtime, term: Eterm) -> String {
        let rendered = runtime.format_term_w(term, None).expect("a rendered term");
        rendered
            .strip_suffix('\n')
            .map(str::to_owned)
            .unwrap_or(rendered)
    }

    /// Drives one `{'$gen_call', {From, Tag}, Request}` through the real service seam
    /// (`serve_file_message`, the `gen:call/4` wire `file.erl`'s `call/2` produces) and
    /// returns the reply **value**.
    fn call_service(runtime: &mut Runtime, table: &mut ProcessTable, request: Eterm) -> Eterm {
        let service = runtime.file_services.server();
        let caller = runtime.self_pid();
        let payload = build(runtime, |ctx| {
            let tag = ctx.atom("$gen_call").expect("an atom");
            let from =
                terms::make_tuple(ctx, &[caller.to_eterm(), make_small(1)]).expect("a tuple");
            terms::make_tuple(ctx, &[tag, from, request]).expect("a tuple")
        });
        let (_, envelope) = runtime
            .serve_file_message(table, service, payload)
            .expect("a served request")
            .expect("a `$gen_call` is always answered");
        terms::tuple_element(envelope, 1).expect("the reply element")
    }

    /// The io-device pid of an `{ok, IoPid}` reply.
    fn device_of(runtime: &mut Runtime, reply: Eterm) -> ProcessId {
        let fields = tuple_fields(reply).expect("an `{ok, IoPid}` tuple");
        assert_eq!(
            runtime.atom_name(fields[0]).as_deref(),
            Some("ok"),
            "expected a live io device, got {}",
            render(runtime, reply)
        );
        ProcessId::from_eterm(fields[1]).expect("the device pid")
    }

    /// Serves one raw device request (`{put_chars,latin1,Bin}` / `close`) through the real
    /// `serve_device_request` seam.
    fn device_request(runtime: &mut Runtime, device: ProcessId, request: Eterm) -> (Eterm, bool) {
        runtime
            .serve_device_request(device, request)
            .expect("a served device request")
    }

    /// `{put_chars, latin1, Payload}` — the io request `file:write/2` sends
    /// (`file.erl:1797-1803`'s `io:request(File, {put_chars,latin1,Bin})`).
    fn put_chars_request(runtime: &mut Runtime, payload: Eterm) -> Eterm {
        build(runtime, |ctx| {
            let put_chars = ctx.atom("put_chars").expect("an atom");
            let latin1 = ctx.atom("latin1").expect("an atom");
            terms::make_tuple(ctx, &[put_chars, latin1, payload]).expect("a tuple")
        })
    }

    /// `{write_file, Name, Bin}` is `?PRIM_FILE:write_file(Name, Bin)`
    /// (`file_server.erl:111-112`), the body of `file:write_file/2`
    /// (`file.erl:1193-1194`): it answers `ok` and really writes the bytes.
    #[test]
    fn write_file_creates_the_path_and_writes_the_bytes() {
        let mut runtime = runtime();
        let mut table = table(&mut runtime);
        let file = TempPath::new("write_file");
        let name = path_term(&mut runtime, file.path());
        let request = write_file_request(&mut runtime, name, b"hello");
        let reply = runtime
            .serve_server_request(&mut table, request)
            .expect("a served request")
            .expect("the oracle answers `{reply, …}`");
        assert_eq!(render(&mut runtime, reply), "ok");
        assert_eq!(
            std::fs::read(file.path()).expect("the written file"),
            b"hello"
        );
    }

    /// The oracle's failure vocabulary for the same arm: an atom path and a non-iodata
    /// payload are the **value** `{error, badarg}`, and a directory is the `open(2)` errno
    /// `{error, eisdir}` — never a fabricated `ok`.
    #[test]
    fn write_file_reports_badarg_and_eisdir() {
        let mut runtime = runtime();
        let mut table = table(&mut runtime);

        // A code point above 255 is not `iodata`.
        let file = TempPath::new("write_file_bad_payload");
        let bad = build(&mut runtime, |ctx| {
            let write_file = ctx.atom("write_file").unwrap();
            let name = ctx.charlist(file.path().to_str().unwrap()).unwrap();
            terms::make_tuple(ctx, &[write_file, name, make_small(300)]).unwrap()
        });
        let reply = runtime
            .serve_server_request(&mut table, bad)
            .expect("a served request")
            .expect("a reply");
        assert_eq!(render(&mut runtime, reply), "{error,badarg}");

        // An atom is not a path (`prim_file:encode_path/1` rejects it).
        let atom = build(&mut runtime, |ctx| ctx.atom("nope").unwrap());
        let request = write_file_request(&mut runtime, atom, b"x");
        let reply = runtime
            .serve_server_request(&mut table, request)
            .expect("a served request")
            .expect("a reply");
        assert_eq!(render(&mut runtime, reply), "{error,badarg}");

        // A directory is `EISDIR` — `open(2)`'s own answer for the write flags.
        let dir = unique_path("write_file_dir");
        std::fs::create_dir_all(&dir).expect("a private temporary directory");
        let name = path_term(&mut runtime, &dir);
        let request = write_file_request(&mut runtime, name, b"x");
        let reply = runtime
            .serve_server_request(&mut table, request)
            .expect("a served request")
            .expect("a reply");
        assert_eq!(render(&mut runtime, reply), "{error,eisdir}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full `file:open/2` + `file:write/2` + `file:close/1` round trip, through the
    /// **service** seam for the open and the **device** seam for the write and the close:
    /// the bytes land on disk and the close retires the device.
    #[test]
    fn an_open_write_round_trip_lands_on_disk() {
        let mut runtime = runtime();
        let mut table = table(&mut runtime);
        let file = TempPath::new("round_trip");
        let name = path_term(&mut runtime, file.path());
        let modes = mode_list(&mut runtime, &["write"]);
        let request = open_request(&mut runtime, name, modes);
        let reply = call_service(&mut runtime, &mut table, request);
        let device = device_of(&mut runtime, reply);

        let payload = build(&mut runtime, |ctx| {
            terms::make_heap_binary(ctx, b"round trip").unwrap()
        });
        let request = put_chars_request(&mut runtime, payload);
        let (reply, retire) = device_request(&mut runtime, device, request);
        assert_eq!(render(&mut runtime, reply), "ok");
        assert!(!retire, "a `put_chars` never retires the device");

        let close = build(&mut runtime, |ctx| ctx.atom("close").unwrap());
        let (reply, retire) = device_request(&mut runtime, device, close);
        assert_eq!(render(&mut runtime, reply), "ok");
        assert!(retire, "`file:close/1` retires the device");

        assert_eq!(
            std::fs::read(file.path()).expect("the written file"),
            b"round trip"
        );
    }

    /// A write-mode open now **creates** a missing path — the flag derivation the floor's
    /// `prim_file:open/2` owns (`O_WRONLY|O_CREAT|O_TRUNC`, `unix_prim_file.c:152-166`) —
    /// while a read-mode open on a missing path is still the oracle's `{error, enoent}`.
    #[test]
    fn a_write_open_creates_and_a_read_open_reports_enoent() {
        let mut runtime = runtime();
        let mut table = table(&mut runtime);

        let file = TempPath::new("write_creates");
        assert!(!file.path().exists(), "the fixture path must be absent");
        let name = path_term(&mut runtime, file.path());
        let modes = mode_list(&mut runtime, &["write"]);
        let request = open_request(&mut runtime, name, modes);
        let reply = call_service(&mut runtime, &mut table, request);
        let _ = device_of(&mut runtime, reply);
        assert!(
            file.path().exists(),
            "a `[write]` open creates the path under both runtimes"
        );

        let missing = TempPath::new("read_absent");
        let name = path_term(&mut runtime, missing.path());
        let modes = mode_list(&mut runtime, &["read"]);
        let request = open_request(&mut runtime, name, modes);
        let reply = call_service(&mut runtime, &mut table, request);
        assert_eq!(render(&mut runtime, reply), "{error,enoent}");
    }

    /// A `valid_enc/1`-rejected encoding is the caller's `{error, badarg}`
    /// (`file_io_server.erl:159-178`) and is decided **before** the open, so it never
    /// creates — or truncates — the path.
    #[test]
    fn an_invalid_encoding_is_rejected_before_the_open() {
        let mut runtime = runtime();
        let mut table = table(&mut runtime);
        let file = TempPath::new("bad_encoding");
        let name = path_term(&mut runtime, file.path());
        let modes = build(&mut runtime, |ctx| {
            let key = ctx.atom("encoding").unwrap();
            let value = ctx.atom("bogus").unwrap();
            let pair = terms::make_tuple(ctx, &[key, value]).unwrap();
            terms::make_list(ctx, &[pair]).unwrap()
        });
        let request = open_request(&mut runtime, name, modes);
        let reply = call_service(&mut runtime, &mut table, request);
        assert_eq!(render(&mut runtime, reply), "{error,badarg}");
        assert!(
            !file.path().exists(),
            "the rejected encoding never reaches `open(2)`"
        );
        // The accepted forms are not rejected (`valid_enc/1`'s whole table).
        for modes in [&["latin1"][..], &["unicode"], &["utf8"]] {
            let modes = mode_list(&mut runtime, modes);
            // A rejected encoding would be `badarg` here; these must reach `open(2)` and so
            // report the absent path instead.
            let file = TempPath::new("valid_encoding");
            let name = path_term(&mut runtime, file.path());
            let request = open_request(&mut runtime, name, modes);
            let reply = call_service(&mut runtime, &mut table, request);
            assert_eq!(render(&mut runtime, reply), "{error,enoent}");
        }
    }

    /// The 2-arity `read_file_info`/`read_link_info` forms (`file_server.erl:144-145`,
    /// `:159-160`) reach the same `read_info_1/3`, whose `Opts` is read from `args.get(1)`.
    #[test]
    fn the_read_file_info_opt_form_reaches_the_same_reader() {
        let mut runtime = runtime();
        let mut table = table(&mut runtime);
        let missing = TempPath::new("info_opts");
        let empty = build(&mut runtime, |ctx| terms::make_list(ctx, &[]).unwrap());
        for command in ["read_file_info", "read_link_info"] {
            let name = path_term(&mut runtime, missing.path());
            let request = build(&mut runtime, |ctx| {
                let command = ctx.atom(command).unwrap();
                terms::make_tuple(ctx, &[command, name, empty]).unwrap()
            });
            let reply = runtime
                .serve_server_request(&mut table, request)
                .expect("a served request")
                .expect("a reply");
            assert_eq!(
                render(&mut runtime, reply),
                "{error,enoent}",
                "`{command}/2`"
            );
        }
    }

    // -- the live-oracle cross-check ------------------------------------------------------

    /// How long an oracle probe may run before it is killed and reaped.
    const ORACLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// `true` for an existing executable file.
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    /// The oracle `erl`: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH`.
    /// A **filesystem** probe only — the oracle is never run just to locate it.
    fn resolve_oracle() -> Option<PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = PathBuf::from(&value);
            if is_executable(&program) {
                return Some(program);
            }
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| is_executable(candidate))
    }

    /// The oracle's `~w` rendering of `probe` — `erl -noshell -eval 'io:format("~w",[…])'
    /// -s init stop`, **bounded** by [`ORACLE_TIMEOUT`] so a probe can never hang the suite.
    /// `None` (loudly) when the oracle is absent or cannot answer.
    fn oracle_render(oracle: &Path, probe: &str) -> Option<String> {
        use std::process::{Command, Stdio};

        let mut command = Command::new(oracle);
        command
            .arg("-noshell")
            .arg("-eval")
            .arg(format!("io:format(\"~w\", [{probe}])"))
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!(
                    "warning: cannot spawn the oracle `{}`: {error}",
                    oracle.display()
                );
                return None;
            }
        };
        let deadline = std::time::Instant::now() + ORACLE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        eprintln!("warning: the oracle probe outlived the bound and was killed");
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => {
                    eprintln!("warning: cannot poll the oracle probe: {error}");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        match child.wait_with_output() {
            Ok(output) if output.status.success() => Some(
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string(),
            ),
            Ok(output) => {
                eprintln!(
                    "warning: the oracle exited with {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                );
                None
            }
            Err(error) => {
                eprintln!("warning: cannot collect the oracle output: {error}");
                None
            }
        }
    }

    /// Renders `call` under the runtime's own `-eval` (`~w`).
    fn runtime_render(call: &str) -> String {
        let mut runtime = runtime();
        let value = runtime.eval(call).expect(call);
        render(&mut runtime, value)
    }

    /// Runs `template` (whose `{p}` stands for `path`) under the runtime's `-eval` and under
    /// the live oracle, and returns their **shared** `~w` rendering — or `None` (loudly) when
    /// the oracle could not answer.
    fn compare_with_oracle(oracle: &Path, template: &str, path: &Path) -> Option<String> {
        let call = template.replace("{p}", path.to_str().expect("an ASCII temporary path"));
        let Some(expected) = oracle_render(oracle, &call) else {
            eprintln!("warning: the oracle could not answer `{call}` — skipped");
            return None;
        };
        let actual = runtime_render(&call);
        assert_eq!(
            actual, expected,
            "`{call}` renders differently than the oracle"
        );
        Some(expected)
    }

    /// Every write-path answer re-run against the **live** oracle: the rendered values and
    /// the bytes on disk must both agree. Skips **loudly** when no oracle is installed.
    #[test]
    fn the_write_answers_match_the_live_oracle() {
        let Some(oracle) = resolve_oracle() else {
            eprintln!();
            eprintln!("========== beam_rt file_server write oracle SKIPPED ==========");
            eprintln!("no `erl` on $PATH (and no executable $ERTS_ORACLE): the oracle");
            eprintln!("comparison cannot run. Set $ERTS_ORACLE (or install Erlang/OTP)");
            eprintln!("and re-run. NOT a pass.");
            eprintln!("==============================================================");
            eprintln!();
            return;
        };
        let dir = unique_path("oracle_write");
        std::fs::create_dir_all(&dir).expect("a private temporary directory");

        // `file:write_file/2` — the service's `{write_file, Name, Bin}` arm.
        let path = dir.join("write2.bin");
        let Some(rendered) =
            compare_with_oracle(&oracle, "file:write_file(\"{p}\", <<\"hello\">>)", &path)
        else {
            return;
        };
        assert_eq!(rendered, "ok");
        assert_eq!(std::fs::read(&path).expect("the written file"), b"hello");

        // `file:write_file/3` — its `open([binary,write])` + `write` + `close` over the
        // service and device seams.
        let path = dir.join("write3.bin");
        let Some(rendered) =
            compare_with_oracle(&oracle, "file:write_file(\"{p}\", <<\"abc\">>, [])", &path)
        else {
            return;
        };
        assert_eq!(rendered, "ok");
        assert_eq!(std::fs::read(&path).expect("the written file"), b"abc");

        // The explicit `file:open/2` + `file:write/2` + `file:close/1` sequence.
        let path = dir.join("seq.bin");
        let Some(rendered) = compare_with_oracle(
            &oracle,
            "begin {ok,F}=file:open(\"{p}\",[write]), R1=file:write(F,<<\"zig\">>), \
             R2=file:close(F), {R1,R2} end",
            &path,
        ) else {
            return;
        };
        assert_eq!(rendered, "{ok,ok}");
        assert_eq!(std::fs::read(&path).expect("the written file"), b"zig");

        // A write-mode open **creates** a missing path.
        let path = dir.join("open_write.bin");
        let Some(rendered) =
            compare_with_oracle(&oracle, "element(1, file:open(\"{p}\", [write]))", &path)
        else {
            return;
        };
        assert_eq!(rendered, "ok");
        assert!(path.exists(), "the `[write]` open created the path");

        // ... and an append-mode one too (`O_CREAT` without `O_TRUNC`).
        let path = dir.join("open_append.bin");
        let Some(rendered) =
            compare_with_oracle(&oracle, "element(1, file:open(\"{p}\", [append]))", &path)
        else {
            return;
        };
        assert_eq!(rendered, "ok");
        assert!(path.exists(), "the `[append]` open created the path");

        // A read-mode open on a missing path is still `{error, enoent}`.
        let path = dir.join("absent");
        let Some(rendered) = compare_with_oracle(&oracle, "file:open(\"{p}\", [read])", &path)
        else {
            return;
        };
        assert_eq!(rendered, "{error,enoent}");

        // An invalid encoding is `{error, badarg}` **before** the open, on both sides.
        let path = dir.join("bad_encoding.bin");
        let Some(rendered) =
            compare_with_oracle(&oracle, "file:open(\"{p}\", [{encoding,bogus}])", &path)
        else {
            return;
        };
        assert_eq!(rendered, "{error,badarg}");
        assert!(
            !path.exists(),
            "the rejected encoding never reached `open(2)`"
        );

        // A directory is `{error, eisdir}` for both the write BIF and the write open.
        let Some(rendered) =
            compare_with_oracle(&oracle, "file:write_file(\"{p}\", <<\"x\">>)", &dir)
        else {
            return;
        };
        assert_eq!(rendered, "{error,eisdir}");

        // `file:read_file/1` reads back a file this runtime wrote through the same arms.
        let path = dir.join("readback.bin");
        std::fs::write(&path, b"on disk").expect("a fixture file");
        let Some(rendered) = compare_with_oracle(&oracle, "file:read_file(\"{p}\")", &path) else {
            return;
        };
        assert_eq!(rendered, "{ok,<<111,110,32,100,105,115,107>>}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
