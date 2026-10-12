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
//! # The stashed read buffer (`State#state.buf`)
//!
//! `{get_line, …}`/`{get_until, …}` read *ahead* — they pull more bytes off the device than they
//! return — and what they read past the reply is the device's buffer for the next request
//! (the oracle's `State#state.buf`). Three arms therefore share that one buffer: the next
//! `{get_chars, …}` (this module), `{get_line, …}` and `{get_until, …}` all consume it **before**
//! any fresh read, `{position, cur}`/`{position, {cur, Offs}}` compensates for it (the physical
//! file offset is ahead of the logical position by exactly the buffer's length), and a successful
//! seek clears it (`std_reply/2`, `file_io_server.erl:344-348`). Every read here reads only what
//! it still needs, so that invariant holds for an arbitrary mix of the three requests.
//!
//! # The `{setopts, …}`/`getopts` read-mode surface
//!
//! Served by the [`setopts`] child module: `io:getopts/1` sends the bare atom `getopts` and
//! `io:setopts/2` sends `{setopts, Opts}` (`file_io_server.erl:395-401`), and both arms read
//! and write the same `Device` fields the `{get_chars, …}`/`{get_line, …}`/`{get_until, …}`
//! readers use, so a mode change is visible to the next read exactly as it is on the oracle.
//! The child's docs carry the wire-level notes.
//!
//! # Honest capability gaps (never a fabricated term)
//!
//! * `{read_file_info, Name}` and `{read_link_info, Name}` are **partly** served by the
//!   [`file_info`] child: the `stat(2)`/`lstat(2)` failure is the oracle's `{error, Reason}`
//!   verbatim, while a *successful* stat — whose reply would need the `#file_info{}` `access`
//!   slot — stays the typed capability gap (the child's docs carry the full argument).
//! * A `{setopts, …}` encoding with no counterpart here is the typed gap, never a fabricated
//!   `ok` (the [`setopts`] child carries the exact rule, and the *open-time* equivalent keeps
//!   its documented `latin1` divergence).
//! * Every *other* request this module does not serve is reported as the typed
//!   [`crate::error::unsupported`] gap rather than a plausible-looking wrong answer
//!   (`pread`/`pwrite`, `advise`, `allocate`, `read_handle_info`, the legacy `put_chars`
//!   arities, a tuple encoding).
//! * An unknown `file_server` `handle_call` request is **answered** here with the typed
//!   `{error, unsupported}` rather than the oracle's silent `{noreply, State}`: a
//!   well-formed `{'$gen_call', …}` must never leave its caller parked with no reply (see
//!   [`Runtime::serve_file_message`]).
//!
//! # Retirement: a request to a retired device is never lost
//!
//! A device that retires (`close`, or a device-killing request) stops being a device, so a
//! request that reaches it — already queued in **its** mailbox, or arriving in a **later**
//! pass — is answered, rather than parked forever, with the oracle's `{error, terminated}`.
//! The pid therefore stays in the drain coverage of [`Runtime::service_file_requests`]
//! ([`FileServices::retired`]) for the **runtime's lifetime**: nothing can know that no
//! further request will ever arrive, so any rule that dropped the pid once its mailbox looked
//! empty would silently lose a request sent afterwards (its sender would park in
//! `file_request/2`'s untimed `receive`, so the run would quiesce to a typed `blocked()` gap
//! instead of surfacing the orphaned request). The cost is bounded by the number of retired
//! devices — the same order as the io-device `Process` records the process table already
//! retains forever (they are never removed from it either). A `{file_reply,…}` is **counted**
//! in that step's delivery total, so a pass that only answers a retired device can never look
//! quiescent.

use std::collections::{HashMap, HashSet};
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
mod setopts;

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
    /// The **retired** io-device pids: their `Device` is gone (the oracle's device process
    /// has died), but a request may still be queued in the pid's mailbox — or arrive in a
    /// **later** scheduler pass. They stay in the drain coverage of
    /// [`Runtime::service_file_requests`] for the runtime's lifetime and are answered with
    /// the oracle's `{error, terminated}`; the set is bounded by the number of retired
    /// devices, the same order as the io-device `Process` records the process table already
    /// retains forever.
    retired: HashSet<ProcessId>,
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
    /// The leftover the device's read-ahead stashed (`State#state.buf`): the bytes a
    /// `{get_line,…}`/`{get_until,…}` request read past what it returned. The next
    /// `{get_chars,…}`/`{get_line,…}`/`{get_until,…}` request consumes it **before** any fresh
    /// read, `{position,cur}` compensates for it, and a successful seek clears it — so
    /// `logical position + buf.len()` is always the physical file offset.
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
            retired: HashSet::new(),
        })
    }

    /// The registered `file_server_2` pid.
    pub(crate) const fn server(&self) -> ProcessId {
        self.server
    }

    /// Whether `service` is an io-device that has **retired** (`close`, or a device-killing
    /// request): its `Device` is gone, so every further request is the oracle's
    /// `{error, terminated}`.
    fn is_retired(&self, service: ProcessId) -> bool {
        self.retired.contains(&service)
    }

    /// Retires the io-device `service`: its open file is dropped exactly as the oracle's
    /// device process dying closes the descriptor, and the pid is kept in drain coverage for
    /// the runtime's lifetime (see [`FileServices::retired`]). A request that lands in its
    /// mailbox at any later moment is answered `{error, terminated}`, never parked.
    fn retire(&mut self, service: ProcessId) {
        self.devices.remove(&service);
        self.retired.insert(service);
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
        // The servers are collected first so `self` is free while `table` is borrowed. A
        // **retired** device is still scanned: a request queued in its mailbox at the moment
        // it retired — or arriving in **any later** pass — must be answered
        // (`{error, terminated}`), never parked forever. The coverage is permanent (see the
        // module docs): a retired pid is never dropped from it.
        let mut servers = Vec::with_capacity(
            1 + self.file_services.devices.len() + self.file_services.retired.len(),
        );
        servers.push(self.file_services.server());
        servers.extend(self.file_services.devices.keys().copied());
        servers.extend(self.file_services.retired.iter().copied());
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
            // A device that has retired answers every further request the way the oracle's
            // dead io-device process does: its caller monitors it, so `file_request/2`
            // (`lib/kernel/src/file.erl:3068-3077`) and `io:request/3` take their `'DOWN'`
            // arm and answer `{error, terminated}`. This native device outlives its retire,
            // so that reply is delivered directly instead of synthesising a `'DOWN'`. This
            // is the reply of `device_mut`'s "closed io device" condition.
            if self.file_services.is_retired(service) {
                let terminated = self.atom("terminated")?;
                let reply = self.error_of(terminated)?;
                let tag_atom = self.atom(reply_tag)?;
                let envelope = self.tuple(&[tag_atom, reply_as, reply])?;
                return Ok(Some((from, envelope)));
            }
            let (reply, retire) = self.serve_device_request(service, fields[3])?;
            if retire {
                self.file_services.retire(service);
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
                // The oracle's `handle_call(Request, From, State) -> {noreply, State}`
                // catch-all (`file_server.erl:197-201`) replies nothing, leaving the caller
                // of a well-formed `{'$gen_call', …}` parked forever in `gen:call/4`'s
                // untimed `receive`. This runtime never parks a caller silently: such a
                // request is answered with the typed `{error, unsupported}` instead.
                None => {
                    let unsupported = self.atom("unsupported")?;
                    self.error_of(unsupported)?
                }
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
    /// `ModeList` is tested with the `is_list/1` **BIF** predicate, so `[]` (a proper empty
    /// list) is accepted and an improper tail is not (`beam_term::is_list` is the cons-cell
    /// predicate and is false for `NIL`).
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
        // `file_server.erl:104-106` guards on `is_list(ModeList)` — the BIF, which is true
        // for `[]` — and answers `{error, einval}` for anything else.
        if terms::list_elements(args[1]).is_err() {
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
                // `file_io_server:file_request({position,At}, _)` (`:319-326`) compensates the
                // request for the stashed buffer (`position/3`, `:970-977`) and then always
                // replies through `std_reply/2` (`:344-348`), which clears `State#state.buf`.
                let buffered = self.device_mut(service)?.buf.len();
                let seek = self.position_seek(args[0], buffered)?;
                let file = self.device_file(service)?;
                let position = file.seek(seek).map_err(io_error)?;
                self.device_mut(service)?.buf.clear();
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
            // The mutable read-mode/encoding surface: `io:getopts/1` sends the bare atom
            // `getopts`, `io:setopts/2` sends `{setopts, Opts}`
            // (`file_io_server.erl:395-401`).
            ("getopts", 0) => self.reply_getopts(service),
            ("setopts", 1) => self.reply_setopts(service, args[0]),
            _ => Err(error::unsupported(&format!(
                "file_server: the `{command}/{}` io request is not implemented",
                args.len()
            ))),
        }
    }

    /// `{get_chars, Enc, _Prompt, N}` → `Data | eof` (`file_io_server.erl:506-535`).
    ///
    /// Only the byte-oriented `latin1` encoding is served: a request in any other encoding
    /// would need `unicode:characters_to_binary/3`, which this native process does not host.
    ///
    /// The device's stashed leftover (`State#state.buf`, the bytes a preceding
    /// `{get_line,…}`/`{get_until,…}` read ahead and did not consume) is served **first**, so
    /// any mix of the three requests on one device agrees with the oracle. A request for `0`
    /// bytes answers the empty cast and leaves that leftover untouched (`get_chars(0, Enc,
    /// State)`, `:506-507`); otherwise `min(N, buf)` is taken off the front, the remainder is
    /// read from the file until `N` bytes are collected or the read hits end of file, and a
    /// request that obtained **nothing at all** answers the atom `eof` (the oracle's
    /// `eof when BufSize =:= 0`, `:529-530`). The reply is a binary or a char list of small
    /// ints per the device's read mode (`cast/4`, `:782-802`).
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
        if count == 0 {
            // `get_chars(0, Enc, State) -> {reply, cast(<<>>, …), State}` (`:506-507`): the
            // empty cast, with the stashed leftover preserved for the next request.
            let data = if binary {
                self.binary(&[])?
            } else {
                self.list(&[])?
            };
            return Ok((data, false));
        }
        // `get_chars(N, OutEnc, #state{buf=Buf, …})` (`:508-528`): `Buf` first, then a fresh
        // read for what is still missing.
        let device = self.device_mut(service)?;
        let taken = count.min(device.buf.len());
        let mut collected: Vec<u8> = device.buf.drain(..taken).collect();
        if collected.len() < count {
            // Read only what is still needed, so the device's physical offset stays exactly
            // `logical position + buf.len()` — the invariant `position_seek` below relies on
            // (the oracle over-reads by up to `?READ_SIZE_BINARY` and subtracts the buffer
            // again in `position/3`, which is observationally the same).
            let file = &mut device.file;
            let mut remaining = count - collected.len();
            let mut chunk = [0u8; 8192];
            while remaining > 0 {
                let want = remaining.min(chunk.len());
                // A single `read(2)` may be a short read; only `0` means end of file.
                let read = file.read(&mut chunk[..want]).map_err(io_error)?;
                if read == 0 {
                    break;
                }
                collected.extend_from_slice(&chunk[..read]);
                remaining -= read;
            }
        }
        if collected.is_empty() {
            let eof = self.atom("eof")?;
            return Ok((eof, false));
        }
        let data = if binary {
            self.binary(&collected)?
        } else {
            let bytes: Vec<Eterm> = collected
                .iter()
                .map(|byte| make_small(i64::from(*byte)))
                .collect();
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
        Ok(&mut self.device_mut(service)?.file)
    }

    /// The device's read mode (binary vs list).
    fn device_binary(&mut self, service: ProcessId) -> Result<bool> {
        Ok(self.device_mut(service)?.binary)
    }

    /// The device's own entry in the service map.
    ///
    /// An unknown pid is a request for a device that `close` already retired — the same
    /// "closed io device" gap every other accessor reports.
    fn device_mut(&mut self, service: ProcessId) -> Result<&mut Device> {
        self.file_services
            .devices
            .get_mut(&service)
            .ok_or_else(|| error::unsupported("file_server: a request for a closed io device"))
    }

    /// The `SeekFrom` a `{position, At}` request denotes, with `cur`/`{cur, Offs}`
    /// compensated for the stashed buffer — `file_io_server:position/3` (`:970-977`):
    ///
    /// ```text
    /// {cur, Offs} -> {cur, Offs - byte_size(Buf)}
    /// cur         -> {cur, -byte_size(Buf)}
    /// _           -> At
    /// ```
    ///
    /// The compensation is what makes the *logical* position — the one the caller observes —
    /// correct: the device's physical offset is ahead of it by exactly `buf.len()` after a
    /// `{get_line,…}`/`{get_until,…}` read-ahead. Every other form (`bof`/`eof`/a bare
    /// integer/`{bof,N}`/`{eof,N}`) is passed through unchanged.
    fn position_seek(&self, at: Eterm, buffered: usize) -> Result<SeekFrom> {
        let buffered = i64::try_from(buffered).map_err(|_| BeamError::SystemLimit)?;
        if let Some(fields) = tuple_fields(at) {
            if fields.len() == 2
                && is_small(fields[1])
                && self.atom_name(fields[0]).as_deref() == Some("cur")
            {
                return Ok(SeekFrom::Current(signed_value(fields[1]) - buffered));
            }
        } else if self.atom_name(at).as_deref() == Some("cur") {
            return Ok(SeekFrom::Current(-buffered));
        }
        seek_from(at, self)
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
        let reason = self.atom(reason)?;
        self.error_of(reason)
    }

    /// `{error, Reason}` for an already-built `Reason` term — the shape every request
    /// failure the oracle *returns* takes (`file_io_server`'s `{error, Reply, State}` arms).
    fn error_of(&mut self, reason: Eterm) -> Result<Eterm> {
        let error = self.atom("error")?;
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
///
/// The bare `cur`/`{cur, Offs}` forms are parsed here as an un-adjusted relative seek;
/// [`Runtime::position_seek`] is the entry point that compensates them for the device's
/// stashed buffer (`file_io_server:position/3`).
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
mod tests;
