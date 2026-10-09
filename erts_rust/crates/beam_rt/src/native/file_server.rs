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
//! `gen_server:call(file_server_2, {open,Name,ModeList}, infinity)`. Nothing in this
//! runtime was ever registered under that name, so the call died with the oracle-shaped
//! `{noproc,{gen_server,call,[file_server_2,…]}}` and the boot stopped.
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
//! * **The io-device.** A successful `{open,Name,ModeList}` returns `{ok, IoPid}`
//!   (`file_io_server:start_link/3`, `file_io_server.erl:57-96`). `IoPid` speaks the
//!   `{file_request,From,ReplyAs,Request}` → `{file_reply,ReplyAs,Reply}` protocol for
//!   `close`/`{position,At}`/`sync`/`truncate` and the
//!   `{io_request,From,ReplyAs,Request}` → `{io_reply,ReplyAs,Reply}` protocol for
//!   `{get_chars,Enc,_Prompt,N}`/`{put_chars,…}` (`file_io_server.erl:199-233`).
//!
//! # Honest capability gaps (never a fabricated term)
//!
//! * `{get_until,Enc,_Prompt,Mod,Func,XtraArgs}` is answered by
//!   `file_io_server`'s `get_chars(io_lib, get_until, …)`, i.e. by repeatedly applying
//!   `Mod:Func([Continuation|XtraArgs])` — for `io:parse_erl_exprs/4` that is
//!   `erl_scan:tokens/4`, real Erlang parser code. A native service process hosts no
//!   engine, so this request is reported as the typed
//!   [`crate::error::unsupported`] gap rather than a plausible-looking wrong answer; the
//!   bounded boot/scheduler drive turns it into a typed error at the deadline.
//! * The same applies to every request this module does not serve (`get_line`, `setopts`,
//!   `getopts`, `pread`/`pwrite`, `advise`, `allocate`, `read_handle_info`, the legacy
//!   `put_chars` arities, a non-`latin1` encoding, `read_file_info`, `write_file`).
//! * An unknown `file_server` `handle_call` request is the oracle's `{noreply, State}`:
//!   the service replies **nothing** and carries on.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::HeapPolicy;
use beam_process::{ProcessId, ProcessTable};
use beam_term::prelude::*;

use crate::context::RuntimeContext;
use crate::error;
use crate::runtime::Runtime;

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

    /// Whether any native file service exists (always true after construction).
    pub(crate) fn is_running(&self) -> bool {
        true
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
        servers.push(self.file_services.server);
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
                match table.deliver_simple(target, service.to_eterm(), envelope) {
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
            if tag.as_deref() == Some("file_request") || tag.as_deref() == Some("io_request") {
                let Ok(from) = ProcessId::from_eterm(fields[1]) else {
                    return Ok(None);
                };
                let reply_as = fields[2];
                let (reply, retire) = self.serve_device_request(service, &fields[3])?;
                if retire {
                    self.file_services.devices.remove(&service);
                }
                let tag_atom = self.atom(&tag.expect("checked above"))?;
                let envelope = self.tuple(&[tag_atom, reply_as, reply])?;
                return Ok(Some((from, envelope)));
            }
        }
        // The gen_server protocol: {'$gen_call', {Pid, Tag}, Request}.
        if fields.len() == 3 && self.atom_name(fields[0]).as_deref() == Some("$gen_call") {
            let Ok(from) = tuple_fields(fields[1]) else {
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
            let reply = match self.serve_server_request(table, &fields[2])? {
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
            // Everything else — `write_file`, `read_file_info`, `copy`, … — has no faithful
            // native implementation here and is a typed gap, never a wrong value.
            _ => Err(error::unsupported(&format!(
                "file_server: the `{command}/{}` request is not implemented",
                args.len()
            ))),
        }
    }

    /// `{open, Name, ModeList}` (`file_server.erl:101-104`).
    ///
    /// A `ModeList` that is not a list is the oracle's `{error, einval}`; a path
    /// `prim_file` rejects is `{error, badarg}`. A **directory** is rejected *before* the
    /// open, because Linux's `open(2)` succeeds on one while the NIF's `efile_open`
    /// answers `EISDIR`; a missing path is the `{error, enoent}` that unblocks the boot.
    /// A readable regular file yields `{ok, IoPid}` with a fresh native io-device.
    fn reply_open(&mut self, table: &mut ProcessTable, args: &[Eterm]) -> Result<Eterm> {
        if !is_list(args[1]) {
            return self.error_tuple("einval");
        }
        let Some(path) = self.encode_path(args[0]) else {
            return self.error_tuple("badarg");
        };
        let os_path = OsStr::from_bytes(&path);
        match std::fs::metadata(os_path) {
            Err(failure) => self.error_tuple_for(&failure),
            Ok(metadata) if metadata.is_dir() => self.error_tuple("eisdir"),
            Ok(_) => match std::fs::File::open(os_path) {
                Err(failure) => self.error_tuple_for(&failure),
                Ok(file) => {
                    let binary = self.open_is_binary(args[1]);
                    let device = table.spawn(self.file_services.policy.clone())?;
                    self.file_services.devices.insert(device, Device { file, binary });
                    let ok = self.atom("ok")?;
                    self.tuple(&[ok, device.to_eterm()])
                }
            },
        }
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

    /// `file_server:handle_call({list_dir, Name}, …)` → `prim_file:list_dir/1`:
    /// `{ok, Names}` with the entries **sorted** (the oracle's order), or `{error, Reason}`.
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
                names.sort();
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
        let Some(fields) = tuple_fields(request) else {
            return Err(error::unsupported("file_server: a non-tuple io request"));
        };
        let Some(command) = self.atom_name(fields[0]) else {
            return Err(error::unsupported("file_server: an unnamed io request"));
        };
        let args = &fields[1..];
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
        let mut ctx = RuntimeContext::new(&mut *self);
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
fn improper_list_head(term: Eterm) -> Option<Eterm> {
    if !is_list(term) {
        return None;
    }
    let tail = terms::cons_tail(term).ok()?;
    if is_list(tail) {
        return None;
    }
    terms::cons_head(term).ok()
}

/// The `SeekFrom` a `{position, At}` request denotes (`file_io_server.erl:268-273`).
fn seek_from(at: Eterm, runtime: &Runtime) -> Result<SeekFrom> {
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
    fn runtime() -> Runtime {
        let mut runtime = Runtime::with_defaults().expect("a runtime");
        runtime.boot().expect("boot");
        runtime
    }

    /// Builds a term on `runtime`'s arena through a fresh context.
    fn build(runtime: &mut Runtime, f: impl FnOnce(&mut RuntimeContext) -> Eterm) -> Eterm {
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
        assert!(runtime.file_services.is_running());
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
        // An unknown form is the typed gap.
        let at = build(&mut runtime, |ctx| make_small(3));
        assert!(seek_from(at, &runtime).is_err());
    }

    #[test]
    fn tuple_fields_rejects_a_non_tuple() {
        assert!(tuple_fields(make_small(1)).is_none());
    }
}
