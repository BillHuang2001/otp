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

//! The **whole-file-descriptor** family of the runtime-native `prim_file:*` floor
//! (contract **A.7**): `open/2`, `read/2`, `write/2`, `close/1`, `position/2`.
//!
//! These are the primitives `file.erl` delegates a **raw** file to; in the oracle they are
//! the pure-Erlang wrappers of `erts/preloaded/src/prim_file.erl` over the C NIFs of
//! `../../erts/emulator/nifs/{common,unix}/prim_file_nif.c`. This module serves the whole
//! observable contract from the host's own file-system calls (`std::fs`) — a file
//! descriptor is exactly what an OS host owns — so a raw file round-trips end to end.
//! The path-only half of the floor (`get_cwd`, `read_file`, `delete`, …) lives in
//! [`super::prim`]; the dispatch table that reaches both is
//! [`RuntimeContext::prim_file_call`](super::prim::RuntimeContext::prim_file_call).
//!
//! # The handle contract (this runtime's one substitution)
//!
//! `open/2` answers `{ok, Fd}` with `Fd = #file_descriptor{}` =
//! `{file_descriptor, prim_file, Data}` (`lib/kernel/include/file.hrl:64-66`,
//! `prim_file.erl:151-152`). `Data` mirrors the oracle's `build_fd_data/3`
//! (`prim_file.erl:512-527`) with **two** documented differences:
//!
//! * it carries an extra `fd_registry_id => <small integer>` entry, and
//! * the value of `handle` is only ever used as the *presence* required for a decode.
//!
//! `fd_registry_id` is the runtime's private key into its open-file table
//! ([`Runtime::open_files_mut`](crate::Runtime::open_files_mut), an
//! `HashMap<u64, std::fs::File>` owned by the `Runtime` — several runtimes can live in one
//! thread, so a process-global table would let their handles collide). The oracle instead
//! *decodes* `handle` (a NIF resource reference) to find the descriptor; reading that
//! payload here would need `beam_term::boxed_val`, which is `unsafe` and this crate is
//! `#![deny(unsafe_code)]`, so the key is carried in the clear. The value is the reference
//! id minted for `handle` out of the runtime's one reference id space
//! ([`Runtime::refs_mut`](crate::Runtime::refs_mut)), so an id is never reused and a stale
//! descriptor can never name a later descriptor's file.
//!
//! The oracle's `read` shape additionally carries `r_buffer` (a real `prim_buffer`) and
//! `r_ahead_size`; this runtime stores a fresh reference **marker** for `r_buffer` and the
//! read-ahead size for `r_ahead_size`. That is faithful to the structure and harmless to the
//! behaviour: `prim_buffer`'s only export is `on_load/0` in this OTP's own `prim_buffer.erl`
//! source, and `read_1/5`'s first guard (`RBufSz >= RSz`, `prim_file.erl:180-181`) fires for
//! every non-positive request — the read-ahead path is dead code for the descriptor shapes
//! this runtime hands out.
//!
//! Every BIF decodes the argument the same way: the term must be a 3-tuple whose element 0
//! is an atom and whose element 2 is a map, and that map must carry `handle`, `owner` and a
//! non-negative small-integer `fd_registry_id`; the caller must be the recorded `owner`
//! (otherwise the oracle's own `error:not_on_controlling_process`, `prim_file.erl:499-504`);
//! and the id must still be in the table (a **closed** descriptor and an unknown one both
//! answer `{error,einval}`, the NIF's `EFILE_STATE_CLOSED` answer,
//! `prim_file_nif.c:618-621`).
//!
//! # The BIFs (each verified against the `erl` oracle, OTP 29.1.1 / erts-17.1)
//!
//! | BIF | Ground truth | Probe ⇒ result |
//! |---|---|---|
//! | `open/2` | `prim_file.erl:132-141` + `build_fd_data/5` (`:512-527`); `efile_translate_modelist` (`prim_file_nif.c:500-539`) + `get_flags`/`efile_open` (`unix_prim_file.c:142-226`) | `{ok,Fd}`; a missing path⇒`{error,enoent}`; a directory with `[read]`⇒`{error,eisdir}`; `[write,exclusive]` on an existing file⇒`{error,eexist}`; a regular file with `[directory]`⇒`{error,enotdir}`; a bad path (`foo`, `<<"/a",0,"b">>`)⇒`{error,badarg}`; a non-list mode term⇒`{error,badarg}` |
//! | `read/2` | `prim_file.erl:162-205` (`read_1/5`) + `read_nif_impl` (`prim_file_nif.c:632-663`) | `{ok,<<…>>}`; a drained file⇒`eof`; `0`⇒`{ok,<<>>}`; `-1`⇒`{error,badarg}`; a float⇒`{error,einval}`; a bigint⇒`{error,einval}`; a write-only fd⇒`{error,ebadf}`; a directory fd⇒`{error,eisdir}`; a closed/bogus fd⇒`{error,einval}` |
//! | `write/2` | `prim_file.erl:247-263` + `write_nif_impl` (`prim_file_nif.c:664-682`, `enif_inspect_iovec`) | `ok` for `<<…>>`/`"…"`/`[<<"a">>,<<"b">>]`/`[<<"ab">>,[<<"c">>]]`/`[]`/`<<>>`; `{error,badarg}` for `foo`/`[256]`/`[-1]`/`[1.5]`/`[1\|2]`/`<<1:4>>`; a read-only fd⇒`{error,ebadf}`; a directory fd⇒`{error,ebadf}` |
//! | `close/1` | `prim_file.erl:154-160` + `close_nif` (`prim_file_nif.c:612-631`) | `ok`; a second close⇒`{error,einval}`; a malformed descriptor⇒`{error,badarg}` |
//! | `position/2` | `prim_file.erl:306-334` (`position/2` + `position_1/3`) + `efile_seek` (`unix_prim_file.c:502-533`) | `{ok,N}`; `bof`/`cur`/`eof`⇒offset 0; a bare integer `N`⇒`{N,bof}`; `{eof,-1}` on an 11-byte file⇒`{ok,10}`; `{bof,-1}`/`-3`⇒`{error,einval}`; `foo`/`{1,bof}`/`{bof,foo}`/`{bof,1.0}`/`{bof,1,2}`⇒`{error,badarg}`; past EOF⇒`{ok,100}`; a closed fd⇒`{error,einval}` |
//!
//! # The mode list (`open/2`)
//!
//! `read`, `write`, `exclusive`, `append`, `sync`, `directory`, `skip_type_check`,
//! `read_ahead` (bare, ⇒ 65536) and `{read_ahead, N}` are recognised; **every other option
//! is silently ignored**, exactly as `efile_translate_modelist` does (`:516-525`) — `raw`,
//! `binary`, `list`, `ram`, `delayed_write`, `{delayed_write,_,_}`, `compressed`,
//! `{compressed,_}` are accepted and have no effect here. `append`/`exclusive` additionally
//! imply **write** and, with neither `read` nor `write` set, **read** is the default
//! (`:531-536`); the resulting OS flags are `get_flags`' (`unix_prim_file.c:142-171`):
//! `read` only ⇒ `O_RDONLY`; `write` only ⇒ `O_WRONLY|O_CREAT|O_TRUNC`; both ⇒
//! `O_RDWR|O_CREAT`; then `append` ⇒ `O_APPEND` with `O_TRUNC` cleared, `exclusive` ⇒
//! `O_EXCL`, `sync` ⇒ `O_SYNC`, `directory` ⇒ `O_DIRECTORY`. The oracle's post-open type
//! checks are reproduced for real (`efile_open`, `:196-212`): `open(2)` succeeds on a
//! directory on Linux, so a non-`skip_type_check` open of a directory without `directory`
//! is `{error,eisdir}`, and a `directory` open of anything else is `{error,enotdir}`.
//!
//! A bad **path** (whatever `encode_path/1` rejects) and a mode term that is not a proper
//! list are the *value* `{error,badarg}` — the wrapper's own `catch error:badarg`
//! (`prim_file.erl:139-141`), never a raise. A host failure is its `errno`
//! (`error_tuple_for`).
//!
//! # Documented divergences
//!
//! * **A malformed `Data` map is uniformly `{error,badarg}`.** The oracle raises
//!   `{badmatch, Data}` from `get_fd_data/1` when `owner` is absent
//!   (`prim_file.erl:500-504`), raises `function_clause` for a term that is not a
//!   `#file_descriptor{}` at all, and throws `{error,ebadf}` when `handle` is absent on the
//!   read path (`:506-510`); this runtime answers the single `{error,badarg}` value for all
//!   of them. Never a panic, never a fabricated success.
//! * **A non-integer `read/2` size is `{error,badarg}`, not `badarith`.** The oracle's
//!   `read_1/5` computes `RASz + RSz`, which raises `badarith` for an atom, a list or any
//!   other non-number (`prim_file.erl:192`). `read_1/5`'s own guard also fires first for a
//!   **non-positive** size, so the oracle answers `read(ClosedFd, 0)` ⇒ `{ok,<<>>}` and
//!   `read(ClosedFd, -1)` ⇒ `{error,badarg}` without consulting the descriptor at all;
//!   this runtime validates the descriptor (so a closed handle is `{error,einval}`) and
//!   the size before reading.
//! * **The read is one `read(2)`.** `efile_readv` loops until the whole requested extent is
//!   filled or the file ends; for the regular-file stacks this runtime is used on, a single
//!   host read returns the same bytes.
//! * **`r_buffer` is a marker, the handle key is `fd_registry_id`** — see above.
//! * **`close/1` cannot report a close(2) failure.** The descriptor is closed by
//!   `std::fs::File`'s `Drop`, whose result cannot be observed; the oracle forwards
//!   `efile_close`'s errno.
//! * **The iodata walk is iterative and bounded by nesting, not by recursion.** `write/2`
//!   flattens the argument with an explicit pending list (as `erl_unicode.c`'s filename walk
//!   is), so no nesting can exhaust the Rust stack; a nesting deeper than
//!   [`MAX_IODATA_DEPTH`] is `{error,badarg}`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, is_nil, is_small, make_small, signed_value, Eterm};

use super::RuntimeContext;
use crate::error;

/// The `Data` key holding this runtime's private open-file registry id.
///
/// The oracle's map has no such entry: it decodes `handle` (a NIF resource reference)
/// instead. Carrying the key in the clear is what keeps the decode `unsafe`-free — see the
/// module docs.
const REGISTRY_ID_KEY: &str = "fd_registry_id";

/// The deepest nesting `write/2` accepts in its iodata argument.
///
/// The walk itself is iterative (an explicit pending list), so this is not a stack-overflow
/// bound but a sanity limit on a pathological argument — the counterpart of the depth guard
/// a recursive formulation would need.
const MAX_IODATA_DEPTH: u32 = 10_000;

impl RuntimeContext<'_> {
    // -- prim_file:open/2 -----------------------------------------------------

    /// `prim_file:open/2` — the file descriptor the whole raw-file surface is built on
    /// (`prim_file.erl:132-141`, `build_fd_data/3`).
    ///
    /// The mode list is parsed by [`OpenModes`] (the wrapper's `build_fd_data/5` plus the
    /// NIF's `efile_translate_modelist`/`get_flags`), the file is opened through the host's
    /// own `open(2)` with those flags, the post-open type checks are applied, and the
    /// descriptor is built on the runtime's pointer-stable `-eval` heap.
    ///
    /// Oracle: `open(P,[read])`⇒`{ok,Fd}`; a missing path⇒`{error,enoent}`;
    /// `open(Dir,[read])`⇒`{error,eisdir}`; `open(File,[directory])`⇒`{error,enotdir}`;
    /// `open(P,[write,exclusive])` on an existing file⇒`{error,eexist}`;
    /// `open(foo,[read])`/`open(<<"/a",0,"b">>,[read])`⇒`{error,badarg}`;
    /// `open(P,foo)`⇒`{error,badarg}`.
    pub(super) fn prim_open(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = *args.first().ok_or(BeamError::BadArg)?;
        let modes = *args.get(1).ok_or(BeamError::BadArg)?;
        let Some(path) = self.encode_path(name) else {
            return self.error_tuple("badarg");
        };
        let Some(modes) = self.open_modes(modes) else {
            return self.error_tuple("badarg");
        };
        let file = match modes.options().open(std::ffi::OsStr::from_bytes(&path)) {
            Ok(file) => file,
            Err(error) => return self.error_tuple_for(&error),
        };
        // `efile_open`'s post-open type checks (`unix_prim_file.c:196-212`). `open(2)`
        // succeeds on a directory on Linux, so asking for one without `directory` must be
        // rejected here; `O_DIRECTORY` already rejects the reverse, and the explicit check
        // keeps it honest on a platform whose `open(2)` does not.
        if !modes.skip_type_check {
            let is_directory = file.metadata().map(|meta| meta.is_dir()).unwrap_or(false);
            if modes.directory && !is_directory {
                return self.error_tuple("enotdir");
            }
            if !modes.directory && is_directory {
                return self.error_tuple("eisdir");
            }
        }
        // The reference is minted **before** the descriptor is built and registered, so a
        // failure while building it leaves no entry behind (nothing is leaked).
        let (reference, handle) = self.mint_reference()?;
        let id = reference.id();
        let descriptor = self.build_descriptor(&modes, handle, id)?;
        self.runtime.open_files_mut().insert(id, file);
        self.ok_tuple(descriptor)
    }

    // -- prim_file:read/2 -----------------------------------------------------

    /// `prim_file:read/2` — up to `Size` bytes as `{ok, Binary}`, or `eof`
    /// (`prim_file.erl:162-205`, `read_nif_impl`).
    ///
    /// `Size` must be a **non-negative small integer**: a negative one is
    /// `{error,badarg}` (`read_1/5`'s buffer read), a float or a bigint is `{error,einval}`
    /// (`enif_get_int64` fails, `prim_file_nif.c:639-641`), an atom/list is
    /// `{error,badarg}` (documented divergence: the oracle raises `badarith`). `Size = 0` is
    /// legal and answers `{ok,<<>>}`.
    ///
    /// A drained descriptor answers the bare atom `eof` (the NIF's `am_eof`, `:653-655`);
    /// a host failure is its errno, which is what makes a write-only descriptor
    /// `{error,ebadf}` and a directory `{error,eisdir}` — exactly as the oracle does.
    ///
    /// Oracle: `read(Fd,5)`⇒`{ok,<<"hello">>}`; again⇒`{ok,<<" world">>}`; again⇒`eof`;
    /// `read(Fd,0)`⇒`{ok,<<>>}`; `read(Fd,-1)`⇒`{error,badarg}`;
    /// `read(Fd,3.0)`/`read(Fd,1 bsl 80)`⇒`{error,einval}`; a `[write]`/`[append]` fd
    /// ⇒`{error,ebadf}`; a `[directory]` fd⇒`{error,eisdir}`; after `close/1`
    /// ⇒`{error,einval}`.
    pub(super) fn prim_read(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fd = *args.first().ok_or(BeamError::BadArg)?;
        let size = *args.get(1).ok_or(BeamError::BadArg)?;
        let Some(descriptor) = self.decode_descriptor(fd) else {
            return self.error_tuple("badarg");
        };
        self.check_owner(&descriptor)?;
        let count = match self.read_size(size) {
            ReadSize::Bytes(count) => count,
            ReadSize::BadArg => return self.error_tuple("badarg"),
            ReadSize::EInval => return self.error_tuple("einval"),
        };
        // The descriptor borrow stays inside the `match` statement: the `File` is never
        // held across a heap-borrowing `self` call.
        let outcome = match self.runtime.open_files_mut().get_mut(&descriptor.id) {
            None => None,
            Some(file) => Some(read_extent(file, count)),
        };
        let Some(read) = outcome else {
            return self.error_tuple("einval");
        };
        match read {
            // A drained descriptor answers the bare atom `eof` (`am_eof`, `:653-655`) —
            // but only a **positive** request can drain: a zero-byte read is `{ok,<<>>}`
            // wherever the descriptor stands, exactly as `read_1/5`'s zero-size head is.
            Ok(bytes) if bytes.is_empty() && count > 0 => self.atom("eof"),
            // The contents are built on the runtime's long-lived `-eval` heap, the same
            // `beam_codec::term::make_binary` `prim_file:read_file/1` uses (and, unlike
            // `beam_engine::terms::make_heap_binary`, with no 64-byte on-heap limit).
            Ok(bytes) => {
                let contents = beam_codec::term::make_binary(self.eval_heap(), &bytes)?;
                self.ok_tuple(contents)
            }
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:write/2 ----------------------------------------------------

    /// `prim_file:write/2` — the iodata written, `ok` on success
    /// (`prim_file.erl:247-263`, `write_nif_impl`).
    ///
    /// The argument is flattened exactly as `erlang:iolist_to_iovec/1` does it (see
    /// [`flatten_iodata`]): a byte-aligned binary, a byte, or a proper list of those, at any
    /// nesting. Anything else — an atom, a float, an integer outside `0..=255`, an improper
    /// tail, a non-byte-aligned bitstring — is `{error,badarg}`. An **empty** payload is
    /// `ok` without a host call.
    ///
    /// Oracle: `write(Fd,<<1,2>>)`/`("ab")`/`([<<"a">>,<<"b">>])`/`([<<"ab">>,[<<"c">>]])`
    /// `/`(`[]`)`/`(<<>>)`⇒`ok`; `write(Fd,foo)`/`([256])`/`([-1])`/`([1.5])`/`([1|2])`
    /// `/`(<<1:4>>)⇒`{error,badarg}`; a read-only fd⇒`{error,ebadf}`; a directory
    /// ⇒`{error,ebadf}`; after `close/1`⇒`{error,einval}`.
    pub(super) fn prim_write(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fd = *args.first().ok_or(BeamError::BadArg)?;
        let io_data = *args.get(1).ok_or(BeamError::BadArg)?;
        let Some(descriptor) = self.decode_descriptor(fd) else {
            return self.error_tuple("badarg");
        };
        self.check_owner(&descriptor)?;
        let Some(bytes) = flatten_iodata(io_data) else {
            return self.error_tuple("badarg");
        };
        let outcome = match self.runtime.open_files_mut().get_mut(&descriptor.id) {
            None => None,
            // An empty payload is `ok` without a host call (`efile_writev` with no iovec
            // writes nothing); a closed descriptor is still an error, as the oracle's
            // `close_nif` state check makes it (`{error,einval}`).
            Some(_) if bytes.is_empty() => Some(Ok(())),
            Some(file) => Some(file.write_all(&bytes)),
        };
        match outcome {
            None => self.error_tuple("einval"),
            Some(Ok(())) => self.ok_atom(),
            Some(Err(error)) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:close/1 ----------------------------------------------------

    /// `prim_file:close/1` — the descriptor removed from the runtime's open-file table,
    /// `ok` on success (`prim_file.erl:154-160`, `close_nif`).
    ///
    /// Removing the entry drops the `std::fs::File`, which closes the host descriptor. A
    /// second close, and an unknown id, are the NIF's `EFILE_STATE_CLOSED` answer
    /// `{error,einval}`; a malformed descriptor is `{error,badarg}`.
    ///
    /// Oracle: `close(Fd)`⇒`ok`; `close(Fd)` again⇒`{error,einval}`;
    /// `close(<<>>)`/`close({file_descriptor,prim_file,{}})`⇒`{error,badarg}`.
    pub(super) fn prim_close(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fd = *args.first().ok_or(BeamError::BadArg)?;
        let Some(descriptor) = self.decode_descriptor(fd) else {
            return self.error_tuple("badarg");
        };
        self.check_owner(&descriptor)?;
        let closed = self.runtime.open_files_mut().remove(&descriptor.id).is_some();
        if closed {
            self.ok_atom()
        } else {
            self.error_tuple("einval")
        }
    }

    // -- prim_file:position/2 -------------------------------------------------

    /// `prim_file:position/2` — the descriptor seeked, `{ok, NewPosition}`
    /// (`prim_file.erl:306-334`, `position_1/3`, `efile_seek`).
    ///
    /// The `Loc` grammar is the wrapper's own: the atoms `bof`/`cur`/`eof` mean offset 0, a
    /// bare term means `{bof, Term}`, and a 2-tuple is `{Mark, Offset}` — whose `Mark` must
    /// be one of the three atoms and whose `Offset` must be an integer (`enif_get_int64`,
    /// `seek_nif`). Anything else is `{error,badarg}`. A **negative** offset from `bof`
    /// cannot be expressed as a position and is the kernel's `EINVAL` ⇒ `{error,einval}`
    /// (`efile_seek`'s `lseek`); a `cur`/`eof` seek that would land before the start of the
    /// file fails the same way through the host. Seeking past EOF is legal.
    ///
    /// Oracle: `position(Fd,bof)`⇒`{ok,0}`; `position(Fd,3)`⇒`{ok,3}`;
    /// `position(Fd,{cur,1})`⇒`{ok,1}`; `position(Fd,{eof,-1})` on an 11-byte file
    /// ⇒`{ok,10}`; `position(Fd,100)`⇒`{ok,100}`; `position(Fd,{bof,-1})`/`(Fd,-3)`
    /// ⇒`{error,einval}`; `position(Fd,foo)`/`({1,bof})`/`({bof,foo})`/`({bof,1.0})`
    /// `/`({bof,1,2})⇒`{error,badarg}`; after `close/1`⇒`{error,einval}`.
    pub(super) fn prim_position(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fd = *args.first().ok_or(BeamError::BadArg)?;
        let location = *args.get(1).ok_or(BeamError::BadArg)?;
        let Some(descriptor) = self.decode_descriptor(fd) else {
            return self.error_tuple("badarg");
        };
        self.check_owner(&descriptor)?;
        let Some(seek) = self.seek_target(location) else {
            return self.error_tuple("badarg");
        };
        // `SeekFrom::Start` is `u64`-only, so a negative `bof` offset never reaches the host
        // — it is the `lseek(SEEK_SET, -1)` `EINVAL` the oracle reports.
        let seek = match seek {
            SeekTarget::Bof(offset) if offset < 0 => return self.error_tuple("einval"),
            SeekTarget::Bof(offset) => SeekFrom::Start(offset as u64),
            SeekTarget::Cur(offset) => SeekFrom::Current(offset),
            SeekTarget::Eof(offset) => SeekFrom::End(offset),
        };
        // `position_1/3` wipes the read buffer first; the oracle's `prim_buffer:wipe/1`
        // only resets the (dead, see the module docs) read-ahead buffer, so the seek is the
        // whole observable effect.
        let outcome = match self.runtime.open_files_mut().get_mut(&descriptor.id) {
            None => None,
            Some(file) => Some(match file.seek(seek) {
                Ok(_) => file.stream_position(),
                Err(error) => Err(error),
            }),
        };
        let Some(position) = outcome else {
            return self.error_tuple("einval");
        };
        match position {
            Ok(position) => {
                let value = i64::try_from(position).map_err(|_| {
                    error::unsupported(
                        "prim_file:position/2: file position beyond the signed 64-bit range",
                    )
                })?;
                let value = terms::make_integer(self, value)?;
                self.ok_tuple(value)
            }
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- the descriptor decode ------------------------------------------------

    /// The runtime's private id and recorded owner of a `#file_descriptor{}` argument.
    ///
    /// `None` is the malformed-descriptor failure every BIF answers as the value
    /// `{error,badarg}` (see "Documented divergences").
    fn decode_descriptor(&self, fd: Eterm) -> Option<Descriptor> {
        let data = descriptor_data(fd)?;
        let mut handle = false;
        let mut owner = None;
        let mut id = None;
        for (key, value) in terms::map_entries(data).ok()? {
            match self.atom_name(key).as_deref() {
                Some("handle") => handle = true,
                Some("owner") => owner = Some(value),
                Some(REGISTRY_ID_KEY) => {
                    if !is_small(value) || signed_value(value) < 0 {
                        return None;
                    }
                    id = u64::try_from(signed_value(value)).ok();
                }
                _ => {}
            }
        }
        if !handle {
            return None;
        }
        Some(Descriptor {
            id: id?,
            owner: owner?,
        })
    }

    /// The oracle's `get_fd_data/1` owner test (`prim_file.erl:499-504`): the calling
    /// process must be the one the descriptor was opened by, else the raise
    /// `error:not_on_controlling_process` — which is **not** caught by the wrappers'
    /// `catch error:badarg`, so it really is a raise here too.
    fn check_owner(&self, descriptor: &Descriptor) -> Result<()> {
        if descriptor.owner == self.pid().to_eterm() {
            Ok(())
        } else {
            Err(error::message("not_on_controlling_process"))
        }
    }

    /// The `#file_descriptor{}` an `open/2` answers with (`prim_file.erl:151-152`,
    /// `build_fd_data/3`).
    ///
    /// The `handle` entry is the reference minted for this file, the `owner` entry this
    /// process's own pid, and [`REGISTRY_ID_KEY`] the runtime-table key; the **read shape**
    /// additionally carries the `r_buffer` marker and `r_ahead_size`.
    fn build_descriptor(&mut self, modes: &OpenModes, handle: Eterm, id: u64) -> Result<Eterm> {
        let id_value = i64::try_from(id).map(make_small).map_err(|_| {
            error::unsupported("prim_file:open/2: file handle id beyond the immediate range")
        })?;
        let owner = self.pid().to_eterm();
        let mut pairs = vec![
            (self.atom("handle")?, handle),
            (self.atom("owner")?, owner),
            (self.atom(REGISTRY_ID_KEY)?, id_value),
        ];
        if modes.read_shape() {
            let (_, buffer) = self.mint_reference()?;
            pairs.push((self.atom("r_buffer")?, buffer));
            pairs.push((self.atom("r_ahead_size")?, make_small(modes.read_ahead)));
        }
        // The shared atom table is cloned as an `Rc` handle first, so the borrow of the
        // runtime ends before `make_map` needs the context itself.
        let atoms = self.runtime.code_server.atoms_handle();
        let data = terms::make_map(self, &pairs, &atoms)?;
        let tag = self.atom("file_descriptor")?;
        let module = self.atom("prim_file")?;
        terms::make_tuple(self, &[tag, module, data])
    }

    /// The parsed `Size` of `read/2` (`read_1/5` + `read_nif_impl`).
    ///
    /// A **non-negative** small integer is the byte count; a negative one, an atom, a list
    /// or a nil is `{error,badarg}`; every other non-small term — a float, a bigint — is
    /// `{error,einval}`, the NIF's `enif_is_number`-then-`enif_get_int64` answer.
    fn read_size(&self, size: Eterm) -> ReadSize {
        if is_small(size) {
            let value = signed_value(size);
            return match usize::try_from(value) {
                Ok(value) => ReadSize::Bytes(value),
                Err(_) => ReadSize::BadArg,
            };
        }
        if terms::float_value(size).is_ok() {
            return ReadSize::EInval;
        }
        if is_atom(size) || is_list(size) || is_nil(size) {
            return ReadSize::BadArg;
        }
        ReadSize::EInval
    }

    /// The parsed `Loc` of `position/2` — the wrapper's own clauses (`prim_file.erl:306-325`)
    /// followed by the C's argument checks (`seek_nif`).
    ///
    /// `None` is the C's `badarg`: a `Mark` that is not one of `bof`/`cur`/`eof`, an `Offset`
    /// that is not an integer, or a term the wrapper's clauses cannot turn into a pair
    /// (only a 2-tuple and the three marker atoms are special; every other term is
    /// `{bof, Itself}`, which the integer check then rejects unless it is an integer).
    fn seek_target(&self, location: Eterm) -> Option<SeekTarget> {
        if terms::tuple_arity(location).is_ok() {
            if terms::tuple_arity(location).ok()? != 2 {
                return None;
            }
            let mark = self.atom_name(terms::tuple_element(location, 0).ok()?)?;
            let offset = terms::tuple_element(location, 1).ok()?;
            if !is_small(offset) {
                return None;
            }
            return SeekTarget::new(&mark, signed_value(offset));
        }
        if let Some(mark) = self.atom_name(location) {
            if let Some(target) = SeekTarget::new(&mark, 0) {
                return Some(target);
            }
        }
        // The wrapper's fallthrough clause: `position(Fd, Offset) -> position(Fd, {bof, Offset})`.
        if is_small(location) {
            return Some(SeekTarget::Bof(signed_value(location)));
        }
        None
    }

    /// The parsed `open/2` mode list — the wrapper's `build_fd_data/5` state machine
    /// (`prim_file.erl:512-527`) over the NIF's option vocabulary
    /// (`efile_translate_modelist`, `prim_file_nif.c:500-539`).
    ///
    /// `None` when `modes` is not a **proper** list: the wrapper's `catch error:badarg`
    /// turns that into the `{error,badarg}` value. (The oracle's own NIF silently stops at
    /// an improper tail — the Erlang `file:open/2` rejects it before `prim_file` is reached,
    /// and `prim_file:open/2` aborts the emulator outright. `{error,badarg}` is the honest
    /// answer here.)
    fn open_modes(&self, modes: Eterm) -> Option<OpenModes> {
        let mut parsed = OpenModes::default();
        let mut current = modes;
        loop {
            if is_nil(current) {
                break;
            }
            if !is_list(current) {
                return None;
            }
            let head = terms::cons_head(current).ok()?;
            current = terms::cons_tail(current).ok()?;
            // Atom names are resolved through the **shared** atom table: indices are only
            // comparable inside the table that produced them.
            match self.atom_name(head).as_deref() {
                Some("read") => {
                    parsed.read = true;
                    parsed.shape = FdShape::Read;
                }
                Some("write") => {
                    parsed.write = true;
                    if parsed.shape == FdShape::Default {
                        parsed.shape = FdShape::Write;
                    }
                }
                Some("exclusive") => parsed.exclusive = true,
                Some("append") => parsed.append = true,
                Some("sync") => parsed.sync = true,
                Some("directory") => parsed.directory = true,
                Some("skip_type_check") => parsed.skip_type_check = true,
                Some("read_ahead") => parsed.read_ahead = DEFAULT_READ_AHEAD,
                _ => {
                    if let Some(size) = self.read_ahead_option(head) {
                        parsed.read_ahead = size;
                    }
                    // Every other option (`raw`, `binary`, `ram`, `delayed_write`, …) is
                    // silently ignored, exactly as `efile_translate_modelist` does.
                }
            }
        }
        // `efile_translate_modelist`'s tail (`prim_file_nif.c:531-536`): `append` and
        // `exclusive` are documented as "open for writing", and with neither bit set the
        // mode defaults to read.
        if parsed.append || parsed.exclusive {
            parsed.write = true;
        } else if !(parsed.read || parsed.write) {
            parsed.read = true;
        }
        Some(parsed)
    }

    /// The `N` of a `{read_ahead, N}` mode option (`prim_file.erl:522-523`), `None` for
    /// anything else — including a `{read_ahead, _}` whose payload is not a non-negative
    /// small integer, which the Erlang clause would store verbatim (the value only ever
    /// reaches `r_ahead_size`, which this runtime does not read back).
    fn read_ahead_option(&self, option: Eterm) -> Option<i64> {
        if terms::tuple_arity(option).ok()? != 2 {
            return None;
        }
        let tag = self.atom_name(terms::tuple_element(option, 0).ok()?)?;
        if tag != "read_ahead" {
            return None;
        }
        let size = terms::tuple_element(option, 1).ok()?;
        if !is_small(size) {
            return None;
        }
        let size = signed_value(size);
        if size < 0 {
            return None;
        }
        Some(size)
    }
}

/// The `read_ahead` mode option's read-ahead size (`64 bsl 10`, `prim_file.erl:521`).
const DEFAULT_READ_AHEAD: i64 = 64 * 1024;

/// The runtime's private handle for one open file: what a descriptor decode yields.
struct Descriptor {
    /// The key into [`Runtime::open_files_mut`](crate::Runtime::open_files_mut).
    id: u64,
    /// The pid `open/2` recorded, compared against the caller's own.
    owner: Eterm,
}

/// The `Data` map of a `#file_descriptor{}` argument, or `None` when the term is not a
/// well-shaped descriptor at all (`prim_file.erl:499-504`).
fn descriptor_data(fd: Eterm) -> Option<Eterm> {
    if terms::tuple_arity(fd).ok()? != 3 {
        return None;
    }
    if !is_atom(terms::tuple_element(fd, 0).ok()?) {
        return None;
    }
    let data = terms::tuple_element(fd, 2).ok()?;
    if terms::map_entries(data).is_err() {
        return None;
    }
    Some(data)
}

/// One `read(2)` of at most `count` bytes (`efile_readv`,
/// `unix_prim_file.c:400-440`), with the descriptor's position advanced by what the host
/// returned.
///
/// The destination is pre-sized to `count` — the oracle's `enif_alloc_binary(block_size)`
/// — and a reservation the host cannot satisfy is the NIF's own `ENOMEM` answer
/// (`prim_file_nif.c:642-644`), never an abort.
fn read_extent(file: &mut File, count: usize) -> std::io::Result<Vec<u8>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut buffer: Vec<u8> = Vec::new();
    buffer
        .try_reserve_exact(count)
        .map_err(|_| std::io::Error::from_raw_os_error(libc::ENOMEM))?;
    buffer.resize(count, 0);
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    Ok(buffer)
}

/// The flattened bytes of a `write/2` iodata argument — `erlang:iolist_to_iovec/1`'s walk
/// (`erl_bif_binary.c`), `None` for the `badarg` it raises.
///
/// Accepted: a byte-aligned binary (its bytes), a byte (`0..=255`), and a proper list of
/// those at any nesting. Rejected: any other term, an integer outside a byte's range, an
/// improper tail, a non-byte-aligned bitstring, and a nesting deeper than
/// [`MAX_IODATA_DEPTH`].
///
/// The walk is **iterative** — an explicit pending list carrying each element's nesting
/// depth — so no argument can exhaust the Rust stack. Elements are pushed in reverse so the
/// bytes come out in source order.
fn flatten_iodata(term: Eterm) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut pending: Vec<(Eterm, u32)> = vec![(term, 0)];
    while let Some((current, depth)) = pending.pop() {
        if depth > MAX_IODATA_DEPTH {
            return None;
        }
        if let Ok(binary) = terms::binary_bytes(current) {
            bytes.extend_from_slice(binary);
            continue;
        }
        if is_small(current) {
            let value = signed_value(current);
            if !(0..=255).contains(&value) {
                return None;
            }
            bytes.push(value as u8);
            continue;
        }
        if is_nil(current) {
            continue;
        }
        if !is_list(current) {
            return None;
        }
        let mut cells = Vec::new();
        let mut cell = current;
        loop {
            if is_nil(cell) {
                break;
            }
            if !is_list(cell) {
                return None;
            }
            cells.push(terms::cons_head(cell).ok()?);
            cell = terms::cons_tail(cell).ok()?;
        }
        pending.extend(cells.into_iter().rev().map(|element| (element, depth + 1)));
    }
    Some(bytes)
}

/// Where `position/2` wants the descriptor moved to, after the wrapper's clause selection
/// and the C's argument checks.
enum SeekTarget {
    /// `{bof, Offset}` — relative to the start of the file.
    Bof(i64),
    /// `{cur, Offset}` — relative to the current position.
    Cur(i64),
    /// `{eof, Offset}` — relative to the end of the file.
    Eof(i64),
}

impl SeekTarget {
    /// The target for the atom `mark`, or `None` for a `Mark` the C rejects.
    fn new(mark: &str, offset: i64) -> Option<SeekTarget> {
        match mark {
            "bof" => Some(SeekTarget::Bof(offset)),
            "cur" => Some(SeekTarget::Cur(offset)),
            "eof" => Some(SeekTarget::Eof(offset)),
            _ => None,
        }
    }
}

/// `build_fd_data/5`'s `Mode` accumulator (`prim_file.erl:512-527`): which of the two
/// descriptor shapes the option list produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FdShape {
    /// No `read` and no `write` option yet — the read shape, like `build_fd_data/5`'s
    /// `default` mode.
    #[default]
    Default,
    /// A `read` option was seen.
    Read,
    /// A `write` option was seen while the mode was still `Default`.
    Write,
}

/// The parsed `open/2` mode list.
#[derive(Debug, Default)]
struct OpenModes {
    /// A `read` option appeared.
    read: bool,
    /// A `write` option appeared (or `append`/`exclusive` implied one).
    write: bool,
    /// `append` appeared: `O_APPEND`, and no truncation.
    append: bool,
    /// `exclusive` appeared: `O_EXCL`.
    exclusive: bool,
    /// `sync` appeared: `O_SYNC`.
    sync: bool,
    /// `directory` appeared: `O_DIRECTORY`, plus the type checks' counterpart.
    directory: bool,
    /// `skip_type_check` appeared: the post-open directory checks are skipped.
    skip_type_check: bool,
    /// The accumulated read-ahead size (`read_ahead`/`{read_ahead, N}`), 0 by default.
    read_ahead: i64,
    /// `build_fd_data/5`'s final mode.
    shape: FdShape,
}

impl OpenModes {
    /// Whether the descriptor carries the **read** shape, i.e. `r_buffer` + `r_ahead_size`.
    fn read_shape(&self) -> bool {
        self.shape != FdShape::Write
    }

    /// The host open flags `get_flags` derives (`unix_prim_file.c:142-171`).
    ///
    /// `OpenOptions`' own accessors express `O_RDONLY`/`O_WRONLY`/`O_RDWR` + `O_CREAT` +
    /// `O_TRUNC`; the flag-only bits (`O_APPEND`/`O_EXCL`/`O_SYNC`/`O_DIRECTORY`) go through
    /// `custom_flags`, which `std` ORs into the same `open(2)` call.
    fn options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        if self.read && !self.write {
            options.read(true);
        } else if self.write && !self.read {
            options.write(true).create(true);
            // `[write]` truncates; `append` clears `O_TRUNC` afterwards
            // (`unix_prim_file.c:159-162`).
            if !self.append {
                options.truncate(true);
            }
        } else if self.read && self.write {
            options.read(true).write(true).create(true);
        } else {
            // Unreachable: `open_modes` defaults to READ, as `efile_translate_modelist`
            // does. Kept as the same read-only open rather than an invented failure.
            options.read(true);
        }
        let mut extra = 0i32;
        if self.append {
            extra |= libc::O_APPEND;
        }
        if self.exclusive {
            extra |= libc::O_EXCL;
        }
        if self.sync {
            extra |= libc::O_SYNC;
        }
        if self.directory {
            extra |= libc::O_DIRECTORY;
        }
        if extra != 0 {
            options.custom_flags(extra);
        }
        options
    }
}

/// The `Size` argument of `read/2`.
enum ReadSize {
    /// A non-negative small integer: the byte count.
    Bytes(usize),
    /// A term the wrapper answers `{error,badarg}` for.
    BadArg,
    /// A term the wrapper answers `{error,einval}` for.
    EInval,
}
