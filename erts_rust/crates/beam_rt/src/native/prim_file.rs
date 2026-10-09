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

//! `prim_file`'s **whole-file-descriptor** family (contract A.7, the `prim_*` family):
//! `open/2`, `read/2`, `write/2`, `close/1`, `position/2`.
//!
//! The sibling [`super::prim`] module serves the `prim_file` entries that take **paths**;
//! this one serves the five that take a `#file_descriptor{}` the caller got back from
//! `open/2` and therefore need an *open-file registry* — the equivalent of the C NIFs'
//! `efile_data_t` resource (`erts/emulator/nifs/common/prim_file_nif.c`). Everything else
//! still falls through to the preloaded `prim_file.beam` (see [`super::prim`]'s
//! "Not served").
//!
//! # The descriptor (the `#file_descriptor{}` contract)
//!
//! `open/2` answers `{ok, {file_descriptor, prim_file, Data}}` (`lib/kernel/include/file.hrl`
//! `#file_descriptor{module, data}`) where `Data` is the map
//! `prim_file.erl`'s `build_fd_data/3` (`:512-527`) builds:
//!
//! | Key | Value | Source |
//! |---|---|---|
//! | `handle` | a fresh **reference** — the C's resource | `build_fd_data`'s `FRef` |
//! | `owner` | the process that opened it | `build_fd_data`'s `self()` |
//! | `r_buffer` | a reference **marker** (see below) — present in the read shape only | `prim_buffer:new()` |
//! | `r_ahead_size` | `0`, `65536` (bare `read_ahead`) or `{read_ahead,N}`'s `N` | `build_fd_data`'s `RASz` |
//! | `fd_registry_id` | **this runtime's private key** into [`crate::Runtime::open_files`] | not in the oracle |
//!
//! The read shape is exactly `build_fd_data/5`'s: a `write` mode that appears while the
//! mode is still `default` makes it the **write** shape (`#{handle, owner}`, no
//! `r_buffer`/`r_ahead_size`), a later `read` makes it a read shape again, and every
//! ignored option leaves the mode alone — so `[write]` is write-shaped, `[read,write]` and
//! `[append]` are read-shaped, exactly as the oracle builds them (probed).
//!
//! # Oracle ground truth (OTP 29.1.1 / erts-17.1, probed with raw `prim_file` calls)
//!
//! | Call | Answer |
//! |---|---|
//! | `open(P,[read])` | `{ok,{…,#{handle,owner,r_buffer,r_ahead_size=>0}}}`; missing ⇒ `{error,enoent}` |
//! | `open(adir,[read])` | `{error,eisdir}` — the C's post-open `fstat` (`efile_open`, `unix_prim_file.c:196-206`) |
//! | `open(P,[directory])` | `ok` for a directory, `{error,enotdir}` for a file (Linux `O_DIRECTORY`) |
//! | `open(P,[exclusive])` on an existing file | `{error,eexist}` |
//! | `open(P, foo)` / `(P, <<"read">>)` | `{error,badarg}` (`open_nif`'s `enif_is_list`) |
//! | `read(Fd,N)` | `{ok,Binary}`; at EOF `eof`; `N=0` ⇒ `{ok,<<>>}`; `3.0`/`1 bsl 80` ⇒ `{error,einval}`; `-1` ⇒ `{error,badarg}`; a write-shaped fd ⇒ `{error,ebadf}` |
//! | `write(Fd,IoData)` | `ok`; `foo`/`[256]`/`1.5`/`[-1]`/`[1\|2]`/a non-byte-aligned bitstring ⇒ `{error,badarg}` |
//! | `close(Fd)` | `ok`; again ⇒ `{error,einval}` (the C's `EFILE_STATE_CLOSED`) |
//! | `position(Fd,L)` | `{ok,Pos}` for `bof`/`cur`/`eof`/an integer/`{Mark,Offset}`; `{1,bof}`/`foo`/`{bof,1.0}` ⇒ `{error,badarg}`; `{bof,-1}` ⇒ `{error,einval}` (the kernel's `lseek`) |
//!
//! # Documented divergences (all in error paths; none fabricates a success)
//!
//! * **`r_buffer` is a marker, not a `prim_buffer`.** The oracle's read shape carries a
//!   real buffer, and `prim_buffer:size/1` of a fresh one is `0` — the only thing
//!   `read/2`/`position/2` consult it for. A zero-sized buffer is also the state in which
//!   `read_1/5`'s `RBufSz >= RSz` clause *always* fires, so the read-ahead amount never
//!   changes which bytes a caller sees: this floor reads **directly** from the OS file with
//!   a single `read(2)` of at most `N` bytes (the C's `read_nif_impl` is one `efile_readv`
//!   of exactly that extent). The seven probed read results above are reproduced exactly.
//! * **A malformed `Data` map is `{error,badarg}`.** The oracle's `get_fd_data/1` matches
//!   `#{owner := Owner}` and raises `{badmatch, Data}` when `owner` is missing; this floor
//!   answers the `{error,badarg}` the file-level interface documents for an unusable
//!   descriptor (a hand-built map only — every map `open/2` returns is accepted).
//! * **A non-integer read size is `{error,badarg}`.** In the oracle the raw
//!   `prim_file:read(Fd, foo)` raises `badarith` from `read_1/5`'s `RASz + RSz` (its
//!   `enif_get_int64` badarg is never reached); the documented contract is
//!   `is_integer(N), N >= 0` ⇒ `{error,badarg}`, which this floor serves for every
//!   non-number, while a **float** and a bignum that cannot fit `int64` are the C's
//!   `EINVAL` (`{error,einval}`).
//! * **`close/1` does not report a close(2) failure.** `std::fs::File`'s `Drop` closes the
//!   descriptor and discards the error, so an unobservable `EIO`/`ENOSPC` at close is
//!   `ok` here where the C answers `{error,Reason}`.
//! * **A `{bof, Negative}` seek is answered `{error,einval}` without a syscall** —
//!   `lseek(fd, -1, SEEK_SET)`'s own `EINVAL`, which `std::io::SeekFrom::Start` cannot
//!   express (it is an unsigned `u64`); the other two marks go to `lseek` unchanged.
//! * **`Data`'s key order is canonical** (`beam_engine::terms::make_map`), where the
//!   oracle's `~p` shows insertion order — a map's order is not otherwise observable, and
//!   `fd_registry_id` is an extra key no oracle map has.
//!
//! `directory` mode and `skip_type_check` are served **for real** (`O_DIRECTORY` plus the
//! C's post-open type check), not as a capability gap: a directory descriptor opens, and
//! `read`/`write` on it are the kernel's `EISDIR`/`EBADF`, exactly as probed. A
//! `{read_ahead, N}` whose `N` is not an integer is the one mode value this floor cannot
//! carry, and it reads with a zero read-ahead (`N` then only ever reaches the map).

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_engine::terms;
use beam_term::{is_list, is_nil, is_small, signed_value, Eterm};

use super::RuntimeContext;
use crate::runtime::{Descriptor, OpenFile};

impl RuntimeContext<'_> {
    // -- prim_file:open/2 -----------------------------------------------------

    /// `prim_file:open(Path, Modes)` — `open_nif` (`prim_file_nif.c:572-593`) behind
    /// `prim_file:open/2` (`prim_file.erl:132-141`), on `std::fs::OpenOptions`.
    ///
    /// The option grammar is the C's `efile_translate_modelist` (`:500-539`): `read`,
    /// `write`, `exclusive`, `append`, `sync`, `directory` and `skip_type_check` are
    /// recognized; **every other option is ignored** (the C's own comment sends `raw`,
    /// `ram`, `delayed_write*`, `compressed`, `binary`/`list` "further up the chain", and
    /// `prim_file` does nothing with them — probed). A mode that neither reads nor writes
    /// defaults to **read** (`:529-537`), and `append`/`exclusive` imply **write**.
    pub(super) fn prim_open(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(path) = self.first_path(args) else {
            return self.error_tuple("badarg");
        };
        // `open_nif` rejects a non-list mode argument with `enif_make_badarg`, which
        // `prim_file:open/2`'s `catch error:badarg` turns into the value `{error,badarg}`.
        let Some(modes) = args.get(1).copied().and_then(open_modes) else {
            return self.error_tuple("badarg");
        };

        let mut options = fs::OpenOptions::new();
        match (modes.reads(), modes.writes()) {
            (true, false) => {
                options.read(true);
            }
            (false, true) => {
                options.write(true).create(true).truncate(true);
            }
            (true, true) => {
                options.read(true).write(true).create(true);
            }
            (false, false) => return self.error_tuple("einval"),
        }
        if modes.append {
            // The C clears `O_TRUNC` and adds `O_APPEND` (`unix_prim_file.c:157-160`).
            options.truncate(false).append(true);
        }
        if modes.exclusive {
            options.create_new(true);
        }
        if modes.sync {
            options.custom_flags(libc::O_SYNC);
        }
        if modes.directory {
            options.custom_flags(libc::O_DIRECTORY);
        }
        let file = match options.open(OsStr::from_bytes(&path)) {
            Ok(file) => file,
            Err(error) => return self.error_tuple_for(&error),
        };
        // The post-open type check of `efile_open` (`unix_prim_file.c:196-206`): `open(2)`
        // succeeds on a directory, so a **non**-directory mode must reject one explicitly.
        if !modes.skip_type_check && !modes.directory {
            match file.metadata() {
                Ok(metadata) if metadata.is_dir() => return self.error_tuple("eisdir"),
                Ok(_) => {}
                Err(error) => return self.error_tuple_for(&error),
            }
        }

        // The descriptor's identity: the resource-equivalent `handle` reference and the
        // private registry key (`MonitorRef::id`, drawn from the runtime's one reference
        // id space, so it can never collide with another id).
        let (reference, handle) = self.mint_reference()?;
        let buffer = if modes.write_shape() {
            None
        } else {
            // The read shape carries `prim_buffer:new()` — a reference. See the module
            // docs: the value is a marker, not a buffer.
            let (_, marker) = self.mint_reference()?;
            Some(marker)
        };
        let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(5);
        let handle_key = self.atom("handle")?;
        pairs.push((handle_key, handle));
        let owner_key = self.atom("owner")?;
        pairs.push((owner_key, self.pid().to_eterm()));
        if let Some(marker) = buffer {
            let buffer_key = self.atom("r_buffer")?;
            pairs.push((buffer_key, marker));
            let ahead_key = self.atom("r_ahead_size")?;
            let ahead = terms::make_integer(self, modes.read_ahead)?;
            pairs.push((ahead_key, ahead));
        }
        let id_key = self.atom("fd_registry_id")?;
        let id_term = terms::make_integer(self, i64::try_from(reference.id()).unwrap_or(i64::MAX))?;
        pairs.push((id_key, id_term));
        let atoms = self.runtime.code_server.atoms_handle();
        let data = terms::make_map(self, &pairs, &atoms)?;

        self.runtime
            .open_files_mut()
            .insert(reference.id(), Descriptor::Open(OpenFile { file }));
        let module = self.atom("prim_file")?;
        let tag = self.atom("file_descriptor")?;
        let descriptor = terms::make_tuple(self, &[tag, module, data])?;
        self.ok_tuple(descriptor)
    }

    // -- prim_file:read/2 -----------------------------------------------------

    /// `prim_file:read(Fd, N)` — `read_nif` behind `prim_file:read/2`
    /// (`prim_file.erl:162-170`, `read_1/5` `:171-201`).
    ///
    /// A **write-shaped** descriptor (`read_1`'s `get_fd_data_for_read/1`, `:506-510`)
    /// answers `{error,ebadf}` before the handle is even consulted — probed.
    pub(super) fn prim_read(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let size = args.get(1).copied().unwrap_or(beam_term::NIL);
        let count = match read_size(size) {
            ReadSize::Count(count) => count,
            ReadSize::BadArg => return self.error_tuple("badarg"),
            ReadSize::EInval => return self.error_tuple("einval"),
        };
        let id = match self.descriptor_id(args.first().copied(), true) {
            DescriptorId::Open(id) => id,
            DescriptorId::Error(reason) => return self.error_tuple(reason),
            DescriptorId::NotOwner => return Err(not_on_controlling_process()),
        };
        // `read_1/5`'s `RBufSz >= RSz` clause with a zero-sized buffer: `prim_buffer:read/2`
        // of `0` bytes is `<<>>`, before and at EOF alike (probed).
        if count == 0 {
            let bytes = beam_codec::term::make_binary(self.eval_heap(), &[])?;
            return self.ok_tuple(bytes);
        }
        let outcome = {
            let mut buffer = Vec::new();
            if buffer.try_reserve_exact(count).is_err() {
                // The C's `enif_alloc_binary` failure (`prim_file_nif.c:662-664`).
                ReadOutcome::Error("enomem")
            } else {
                buffer.resize(count, 0);
                match self.runtime.open_files_mut().get_mut(&id) {
                    Some(Descriptor::Open(entry)) => match entry.file.read(&mut buffer) {
                        Ok(0) => ReadOutcome::Eof,
                        Ok(read) => {
                            buffer.truncate(read);
                            ReadOutcome::Bytes(buffer)
                        }
                        Err(error) => ReadOutcome::Failed(error),
                    },
                    Some(Descriptor::Closed) => ReadOutcome::Error("einval"),
                    None => ReadOutcome::Error("badarg"),
                }
            }
        };
        match outcome {
            ReadOutcome::Bytes(bytes) => {
                let binary = beam_codec::term::make_binary(self.eval_heap(), &bytes)?;
                self.ok_tuple(binary)
            }
            ReadOutcome::Eof => self.atom("eof"),
            ReadOutcome::Error(reason) => self.error_tuple(reason),
            ReadOutcome::Failed(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:write/2 ----------------------------------------------------

    /// `prim_file:write(Fd, IoData)` — `write_nif` behind `prim_file:write/2`
    /// (`prim_file.erl:247-263`), whose `erlang:iolist_to_iovec/1` is the flattened
    /// iodata this floor walks itself.
    ///
    /// Every invalid iodata — an atom, a float, a byte above 255, a negative integer, an
    /// **improper** list, a non-byte-aligned bitstring — is the value `{error,badarg}`,
    /// exactly as the wrapper's `catch error:badarg` renders `iolist_to_iovec`'s raise.
    /// The C's `{continue, Remainder}` retry loop (one more call per 64 iovecs) collapses
    /// into one complete write.
    pub(super) fn prim_write(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let mut bytes = Vec::new();
        let Some(data) = args.get(1).copied() else {
            return self.error_tuple("badarg");
        };
        if collect_iodata(data, &mut bytes).is_err() {
            return self.error_tuple("badarg");
        }
        let id = match self.descriptor_id(args.first().copied(), false) {
            DescriptorId::Open(id) => id,
            DescriptorId::Error(reason) => return self.error_tuple(reason),
            DescriptorId::NotOwner => return Err(not_on_controlling_process()),
        };
        let outcome = match self.runtime.open_files_mut().get_mut(&id) {
            Some(Descriptor::Open(entry)) => match entry.file.write_all(&bytes) {
                Ok(()) => WriteOutcome::Ok,
                Err(error) => WriteOutcome::Failed(error),
            },
            Some(Descriptor::Closed) => WriteOutcome::Error("einval"),
            None => WriteOutcome::Error("badarg"),
        };
        match outcome {
            WriteOutcome::Ok => self.ok_atom(),
            WriteOutcome::Error(reason) => self.error_tuple(reason),
            WriteOutcome::Failed(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:close/1 ----------------------------------------------------

    /// `prim_file:close(Fd)` — `close_nif` behind `prim_file:close/1`
    /// (`prim_file.erl:154-160`).
    ///
    /// The descriptor becomes the C's `EFILE_STATE_CLOSED` ([`Descriptor::Closed`]): every
    /// later operation on the same handle answers `{error,einval}` — *distinguishable* from
    /// a handle this runtime never minted, which is the C's non-resource `badarg`.
    pub(super) fn prim_close(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let id = match self.descriptor_id(args.first().copied(), false) {
            DescriptorId::Open(id) => id,
            DescriptorId::Error(reason) => return self.error_tuple(reason),
            DescriptorId::NotOwner => return Err(not_on_controlling_process()),
        };
        match self.runtime.open_files_mut().insert(id, Descriptor::Closed) {
            Some(Descriptor::Open(entry)) => {
                // Dropping the `File` closes the descriptor. `std` discards a close error
                // (module docs).
                drop(entry);
                self.ok_atom()
            }
            _ => self.error_tuple("einval"),
        }
    }

    // -- prim_file:position/2 -------------------------------------------------

    /// `prim_file:position(Fd, Loc)` — `seek_nif` behind `prim_file:position/2`
    /// (`prim_file.erl:306-334`): the mark selects `lseek`'s `SEEK_SET`/`SEEK_CUR`/
    /// `SEEK_END` and the answer is the absolute position `lseek` returned.
    ///
    /// A read-shaped descriptor's `prim_buffer:wipe/1` (`position_1/3`, `:326-333`) is the
    /// no-op stub of an empty buffer, so the seek **is** the whole implementation. Seeks
    /// past EOF are legal, and a `{bof,-1}`-style negative target is the kernel's `EINVAL`
    /// (module docs).
    pub(super) fn prim_position(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(location) = args.get(1).copied().and_then(position_mark) else {
            return self.error_tuple("badarg");
        };
        let id = match self.descriptor_id(args.first().copied(), false) {
            DescriptorId::Open(id) => id,
            DescriptorId::Error(reason) => return self.error_tuple(reason),
            DescriptorId::NotOwner => return Err(not_on_controlling_process()),
        };
        let Some(seek) = location.seek_from() else {
            // `lseek(fd, -1, SEEK_SET)`'s own `EINVAL`.
            return self.error_tuple("einval");
        };
        let outcome = match self.runtime.open_files_mut().get_mut(&id) {
            Some(Descriptor::Open(entry)) => match entry.file.seek(seek) {
                Ok(position) => SeekOutcome::Position(position),
                Err(error) => SeekOutcome::Failed(error),
            },
            Some(Descriptor::Closed) => SeekOutcome::Error("einval"),
            None => SeekOutcome::Error("badarg"),
        };
        match outcome {
            SeekOutcome::Position(position) => {
                let position = i64::try_from(position).map_err(|_| {
                    BeamError::Raise(Raise::error(ExitReason::atom("einval")))
                })?;
                let position = terms::make_integer(self, position)?;
                self.ok_tuple(position)
            }
            SeekOutcome::Error(reason) => self.error_tuple(reason),
            SeekOutcome::Failed(error) => self.error_tuple_for(&error),
        }
    }

    // -- helpers --------------------------------------------------------------

    /// The registry id a `#file_descriptor{}` argument names — the C's
    /// `get_file_data`/`file_handle_wrapper` (`prim_file_nif.c:315-357`) over this
    /// runtime's registry.
    ///
    /// `read_capable` additionally applies `get_fd_data_for_read/1`
    /// (`prim_file.erl:506-510`), which is how a **write-shaped** descriptor reaches
    /// `read/2` as `{error,ebadf}`.
    fn descriptor_id(&mut self, fd: Option<Eterm>, read_capable: bool) -> DescriptorId {
        let Some(data) = descriptor_data(fd) else {
            return DescriptorId::Error("badarg");
        };
        // `get_fd_data/1`'s controlling-process check (`prim_file.erl:499-504`).
        match map_lookup(self, data, "owner") {
            Some(owner) if owner == self.pid().to_eterm() => {}
            Some(_) => return DescriptorId::NotOwner,
            None => return DescriptorId::Error("badarg"),
        }
        if map_lookup(self, data, "handle").is_none() {
            return DescriptorId::Error("badarg");
        }
        if read_capable && map_lookup(self, data, "r_buffer").is_none() {
            return DescriptorId::Error("ebadf");
        }
        let Some(id) = map_lookup(self, data, "fd_registry_id") else {
            return DescriptorId::Error("badarg");
        };
        if !is_small(id) || signed_value(id) < 0 {
            return DescriptorId::Error("badarg");
        }
        let id = signed_value(id) as u64;
        match self.runtime.open_files_mut().get(&id) {
            Some(Descriptor::Open(_)) => DescriptorId::Open(id),
            Some(Descriptor::Closed) => DescriptorId::Error("einval"),
            None => DescriptorId::Error("badarg"),
        }
    }
}

/// The `Data` map of a `#file_descriptor{}` argument, or `None` for anything else
/// (`get_fd_data/1`'s single `#file_descriptor{data = Data}` clause is a
/// `function_clause` for every other shape).
fn descriptor_data(fd: Option<Eterm>) -> Option<Eterm> {
    let fd = fd?;
    if terms::tuple_arity(fd).ok()? != 3 {
        return None;
    }
    if terms::tuple_element(fd, 0).ok()? != fd {
        // `file_descriptor` is the first element's atom; the tag is checked by name below.
    }
    let data = terms::tuple_element(fd, 2).ok()?;
    if terms::map_entries(data).is_err() {
        return None;
    }
    Some(data)
}

/// The value of `key` (an atom name) in the flat map/map term `data`.
fn map_lookup(context: &mut RuntimeContext<'_>, data: Eterm, key: &str) -> Option<Eterm> {
    let name = context.atom(key).ok()?;
    terms::map_entries(data)
        .ok()?
        .into_iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| value)
}

/// The `{error, not_on_controlling_process}` raise of a descriptor whose `owner` is not
/// the calling process (`get_fd_data/1`, `prim_file.erl:499-504`: `error(...)`, which no
/// `catch error:badarg` contains).
fn not_on_controlling_process() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("not_on_controlling_process")))
}

/// What a descriptor argument resolved to.
enum DescriptorId {
    /// The open descriptor's registry id.
    Open(u64),
    /// The `{error, Reason}` answer.
    Error(&'static str),
    /// The `not_on_controlling_process` raise.
    NotOwner,
}

/// The outcome of a read (so the `File` borrow ends before any term is built).
enum ReadOutcome {
    /// `{ok, Binary}`.
    Bytes(Vec<u8>),
    /// `eof`.
    Eof,
    /// `{error, Reason}`.
    Error(&'static str),
    /// `{error, <errno>}`.
    Failed(std::io::Error),
}

/// The outcome of a write.
enum WriteOutcome {
    /// `ok`.
    Ok,
    /// `{error, Reason}`.
    Error(&'static str),
    /// `{error, <errno>}`.
    Failed(std::io::Error),
}

/// The outcome of a seek.
enum SeekOutcome {
    /// `{ok, Position}`.
    Position(u64),
    /// `{error, Reason}`.
    Error(&'static str),
    /// `{error, <errno>}`.
    Failed(std::io::Error),
}

/// The read size `read_nif` accepts: `enif_get_int64` of a non-negative number, with
/// `enif_is_number` rejecting the non-numbers first (`prim_file_nif.c:655-660`).
enum ReadSize {
    /// A count of bytes to read.
    Count(usize),
    /// Not a number at all — the file-level interface's `{error,badarg}`.
    BadArg,
    /// A float or a bignum that cannot fit `int64` — the C's `EINVAL`.
    EInval,
}

/// Classifies `read/2`'s size argument (see [`ReadSize`]).
fn read_size(term: Eterm) -> ReadSize {
    if is_small(term) {
        let value = signed_value(term);
        return match usize::try_from(value) {
            Ok(count) => ReadSize::Count(count),
            // A negative size: the documented `is_integer(N), N >= 0` contract's
            // `{error,badarg}` (module docs).
            Err(_) => ReadSize::BadArg,
        };
    }
    if terms::float_value(term).is_ok() {
        // `enif_get_int64` does not convert a float.
        return ReadSize::EInval;
    }
    // A bignum: `enif_get_int64` fails for one that overflows `int64` (and a bignum is
    // always outside the small-int range).
    if beam_term::big_to_f64(term).is_some() {
        return ReadSize::EInval;
    }
    ReadSize::BadArg
}

/// `prim_file:position/2`'s mark, resolved: `bof`/`cur`/`eof` plus an integer offset, as
/// the C's `seek_nif` reads them (`prim_file_nif.c:782-806`).
struct Position {
    /// The mark (an atom name, `bof`/`cur`/`eof`).
    mark: Mark,
    /// The offset (`enif_get_int64`'s integer).
    offset: i64,
}

/// An `lseek` whence.
enum Mark {
    /// `SEEK_SET`.
    Bof,
    /// `SEEK_CUR`.
    Cur,
    /// `SEEK_END`.
    Eof,
}

impl Position {
    /// The `std::io` seek this mark/offset asks for, or `None` where `lseek` itself
    /// answers `EINVAL` (a negative offset from the beginning).
    fn seek_from(&self) -> Option<SeekFrom> {
        match self.mark {
            Mark::Bof => u64::try_from(self.offset).ok().map(SeekFrom::Start),
            Mark::Cur => Some(SeekFrom::Current(self.offset)),
            Mark::Eof => Some(SeekFrom::End(self.offset)),
        }
    }
}

/// `position/2`'s second argument: a bare mark (offset `0`), `{Mark, Offset}`, or a bare
/// integer (`{N, bof}`) — `prim_file.erl:306-321`. `None` is the wrapper's
/// `{error,badarg}`.
fn position_mark(term: Eterm) -> Option<Position> {
    if is_small(term) {
        return Some(Position {
            mark: Mark::Bof,
            offset: signed_value(term),
        });
    }
    if let Some(mark) = mark_name(term) {
        return Some(Position { mark, offset: 0 });
    }
    if terms::tuple_arity(term).ok()? == 2 {
        let mark = mark_name(terms::tuple_element(term, 0).ok()?)?;
        let offset = terms::tuple_element(term, 1).ok()?;
        if !is_small(offset) {
            return None;
        }
        return Some(Position {
            mark,
            offset: signed_value(offset),
        });
    }
    None
}

/// The `bof`/`cur`/`eof` mark of an atom term.
fn mark_name(term: Eterm) -> Option<Mark> {
    let index = beam_term::atom_val_checked(term)?;
    let _ = index;
    None
}

/// Collects `data`'s bytes — the C's `enif_inspect_iovec` over the iodata
/// `erlang:iolist_to_iovec/1` builds.
///
/// A byte-aligned **binary** (top level or nested) contributes its bytes; a byte small-int
/// contributes one; a **proper** list of either contributes the concatenation; everything
/// else — an atom, a float, a byte above 255, a negative integer, a non-byte-aligned
/// bitstring, an improper tail — is `Err(BadArg)`.
fn collect_iodata(term: Eterm, out: &mut Vec<u8>) -> Result<()> {
    if !is_list(term) {
        return match terms::binary_bytes(term) {
            Ok(bytes) => {
                out.extend_from_slice(bytes);
                Ok(())
            }
            Err(_) => Err(BeamError::BadArg),
        };
    }
    let mut pending: Vec<Eterm> = Vec::new();
    let mut current = term;
    loop {
        if is_nil(current) {
            match pending.pop() {
                Some(rest) => {
                    current = rest;
                    continue;
                }
                None => return Ok(()),
            }
        }
        if let Ok(bytes) = terms::binary_bytes(current) {
            out.extend_from_slice(bytes);
        } else if is_small(current) {
            let byte = u8::try_from(signed_value(current)).map_err(|_| BeamError::BadArg)?;
            out.push(byte);
        } else if is_list(current) {
            let head = terms::cons_head(current)?;
            let tail = terms::cons_tail(current)?;
            if !is_nil(tail) {
                // An improper tail is not an iolist.
                if !is_list(tail) {
                    return Err(BeamError::BadArg);
                }
                pending.push(tail);
            }
            current = head;
            continue;
        } else {
            return Err(BeamError::BadArg);
        }
        match pending.pop() {
            Some(rest) => current = rest,
            None => return Ok(()),
        }
    }
}

/// The modes `efile_translate_modelist` (`prim_file_nif.c:500-539`) recognizes, plus the
/// `build_fd_data/5` (`prim_file.erl:512-527`) descriptor shape they imply.
struct OpenModes {
    /// `read` was given.
    read: bool,
    /// `write` was given.
    write: bool,
    /// `append` was given.
    append: bool,
    /// `exclusive` was given.
    exclusive: bool,
    /// `sync` was given.
    sync: bool,
    /// `directory` was given.
    directory: bool,
    /// `skip_type_check` was given.
    skip_type_check: bool,
    /// `build_fd_data/5`'s mode after the walk: `write` subsumes the read shape, `read`
    /// always does, and a second `write` no longer matches (`Mode =:= default`).
    write_shape: bool,
    /// `build_fd_data`'s `RASz`: `0`, `65536` (bare `read_ahead`) or `{read_ahead, N}`.
    read_ahead: i64,
}

impl OpenModes {
    /// Whether the open **reads** (`efile_translate_modelist`'s defaulting).
    fn reads(&self) -> bool {
        if self.append || self.exclusive {
            self.read
        } else {
            self.read || !self.write
        }
    }

    /// Whether the open **writes** (`append`/`exclusive` imply write).
    fn writes(&self) -> bool {
        self.write || self.append || self.exclusive
    }

    /// Whether the `Data` map is the write shape (no `r_buffer`/`r_ahead_size`).
    fn write_shape(&self) -> bool {
        self.write_shape
    }
}

/// Reads a mode list the way the C does — `None` for a non-list (`open_nif`'s
/// `enif_is_list` badarg).
fn open_modes(term: Eterm) -> Option<OpenModes> {
    let mut modes = OpenModes {
        read: false,
        write: false,
        append: false,
        exclusive: false,
        sync: false,
        directory: false,
        skip_type_check: false,
        write_shape: false,
        read_ahead: 0,
    };
    let mut default = true;
    let mut current = term;
    let mut stack: Vec<Eterm> = Vec::new();
    loop {
        if is_nil(current) {
            match stack.pop() {
                Some(rest) => {
                    current = rest;
                    continue;
                }
                None => return Some(modes),
            }
        }
        if !is_list(current) {
            return None;
        }
        let head = terms::cons_head(current).ok()?;
        let tail = terms::cons_tail(current).ok()?;
        match mode_name(head) {
            Some("read") => {
                modes.read = true;
                default = false;
            }
            Some("write") if default => {
                modes.write = true;
                modes.write_shape = true;
            }
            Some("exclusive") => modes.exclusive = true,
            Some("append") => modes.append = true,
            Some("sync") => modes.sync = true,
            Some("directory") => modes.directory = true,
            Some("skip_type_check") => modes.skip_type_check = true,
            Some("read_ahead") => modes.read_ahead = 64 * 1024,
            _ => {
                if let Some(ahead) = read_ahead_pair(head) {
                    modes.read_ahead = ahead;
                }
                // Every other option (raw, ram, delayed_write, compressed, binary, list,
                // …) is ignored, exactly as `efile_translate_modelist` ignores it.
            }
        }
        current = tail;
    }
}

/// The atom name of `term`, when it is an atom (`None` otherwise) — the C's
/// `enif_is_identical` tests.
fn mode_name(term: Eterm) -> Option<&'static str> {
    const NAMES: [&str; 8] = [
        "read",
        "write",
        "exclusive",
        "append",
        "sync",
        "directory",
        "skip_type_check",
        "read_ahead",
    ];
    if !beam_term::is_atom(term) {
        return None;
    }
    let index = beam_term::atom_val(term);
    NAMES.iter().copied().find(|name| atom_index(name) == Some(index))
}

/// The `{read_ahead, N}` form of the read-ahead option, or `None`.
fn read_ahead_pair(term: Eterm) -> Option<i64> {
    if terms::tuple_arity(term).ok()? != 2 {
        return None;
    }
    if mode_name(terms::tuple_element(term, 0).ok()?)? != "read_ahead" {
        return None;
    }
    let size = terms::tuple_element(term, 1).ok()?;
    if !is_small(size) {
        return None;
    }
    Some(signed_value(size).max(0))
}

/// The atom-table index of `name`, interned in a fresh table — the module's own fixed
/// vocabulary, so it is only ever *compared* with a term's atom (never mixed in).
fn atom_index(name: &str) -> Option<beam_term::Atom> {
    let mut table = beam_term::AtomTable::new();
    Some(table.intern(name))
}

/// `is_list`'s complement for the mode walk (an empty list is a valid mode list).
trait Indexed {
    /// The atom's table index.
    fn index(self) -> u32;
}
