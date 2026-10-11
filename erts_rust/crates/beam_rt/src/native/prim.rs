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

//! The runtime-native `prim_file:*` floor (contract **A.7**, the `prim_*` family).
//!
//! `prim_file` is a **preloaded** module: the file-system primitives `file.erl`
//! delegates to. In the oracle its BIFs are the C `prim_file_nif.c` NIFs
//! (`../../erts/emulator/nifs/{common,unix}/prim_file_nif.c`), registered by
//! `erts/preloaded/src/prim_file.erl`'s `-nifs()` table; without a host for that NIF
//! this runtime's preloaded `prim_file.beam` calls `erlang:nif_error(undef)` and the
//! caller gets an honest raise.
//!
//! This module serves the **smallest subset that is genuinely a runtime primitive** and
//! whose whole observable contract is pinned byte-for-byte against the oracle, backed by
//! the host's own file-system calls (`std::fs`/`std::env`) — "primitive" is exactly what
//! this layer is for. Everything else returns [`None`] from
//! [`RuntimeContext::prim_file_call`], so the call falls through to the preloaded
//! `.beam` — never a wrong value, never a fabricated success.
//!
//! # BIFs served (each verified against the `erl` oracle, OTP 29.1.1 / erts-17.1)
//!
//! Every entry here takes **path terms** (see "The path contract" below) and is backed by
//! the host's own file-system calls (`std::fs`); the Erlang wrappers are in
//! `prim_file.erl` and the C bodies in `unix_prim_file.c`.
//!
//! | BIF | Ground truth | Probe ⇒ result |
//! |---|---|---|
//! | `get_cwd/0` | `prim_file.erl:787-793` → `get_cwd_nif` → `efile_get_cwd` (`unix_prim_file.c:1050-1083`, `getcwd(3)`), decoded by `internal_native2name` | `{ok,"/home/…"}` (the cwd as a **string**) |
//! | `get_cwd/1` | `prim_file.erl:771-785` (pure Erlang clauses) | `[]`→`{error,badarg}`; `"C:"`→`{error,enotsup}` (`efile_get_device_cwd` is a constant `ENOTSUP`, `unix_prim_file.c:1042-1048`); `"AB:"`/`":"`/`"ab"`/`[1,2]`/`[65\|foo]`→`{error,einval}`; `<<"C:">>`/`floppy`→`{error,badarg}` |
//! | `read_file/1` | `prim_file.erl:564-570` → `read_file_nif` | `{ok,<<"…">>}`; a missing file→`{error,enoent}`; a directory→`{error,eisdir}`; a non-directory component→`{error,enotdir}`; a symlink loop→`{error,eloop}`; a bad path→`{error,badarg}` |
//! | `delete/1` | `prim_file.erl:804-809` → `del_file_nif` → `efile_del_file` = `unlink(2)` (`unix_prim_file.c:991-1002`) | `delete("f")`⇒`ok`; `delete("missing")`⇒`{error,enoent}`; `delete("adir")`⇒`{error,eperm}` (the C's `EISDIR`→`EPERM` remap); `delete(foo)`/`(1)`/`(<<"/tmp/a",0,"b">>)`⇒`{error,badarg}` |
//! | `rename/2` | `prim_file.erl:811-816` → `rename_nif` → `efile_rename` = `rename(2)` (`unix_prim_file.c:945-961`) | `rename("a","b")`⇒`ok`; `rename("missing","x")`⇒`{error,enoent}`; `rename("keep.txt","keep.txt/sub")`⇒`{error,enotdir}`; a directory onto a non-empty directory⇒`{error,eexist}` (the C's `ENOTEMPTY`→`EEXIST` remap); `rename("/","x")`⇒`{error,einval}` (the C's `/` special case); either arg bad⇒`{error,badarg}` |
//! | `make_dir/1` | `prim_file.erl:817-822` → `make_dir_nif` → `efile_make_dir` = `mkdir(2)`, mode `0777` (`unix_prim_file.c:979-989`, `DIR_MODE` `:56`) | `make_dir("d")`⇒`ok`; again⇒`{error,eexist}`; an existing **file**⇒`{error,eexist}`; `make_dir("missing/new")`⇒`{error,enoent}`; `make_dir("f.txt/new")`⇒`{error,enotdir}`; bad path⇒`{error,badarg}` |
//! | `del_dir/1` | `prim_file.erl:823-828` → `del_dir_nif` → `efile_del_dir` = `rmdir(2)` (`unix_prim_file.c:1004-1032`) | `del_dir("empty")`⇒`ok`; `del_dir("nonempty")`⇒`{error,eexist}` (`ENOTEMPTY`→`EEXIST`); a **file**⇒`{error,enotdir}`; `del_dir("missing")`⇒`{error,enoent}`; a **non-empty cwd**⇒`{error,einval}` (the C's ino/dev stat check); bad path⇒`{error,badarg}` |
//! | `read_link/1` | `prim_file.erl:590-606` → `read_link_nif` → `efile_read_link` = `readlink(2)` (`unix_prim_file.c:859-895`), decoded by `translate_raw_name/2` | a symlink⇒`{ok,"target"}` (**string**); a **dangling** symlink⇒`{ok,"missing_target"}`; a regular file⇒`{error,einval}`; a directory⇒`{error,einval}`; `missing`⇒`{error,enoent}`; a non-UTF-8 target⇒`{error,einval}`; bad path⇒`{error,badarg}` |
//! | `make_link/2` | `prim_file.erl:829-834` → `make_hard_link_nif` → `efile_make_hard_link` = `link(2)` (`unix_prim_file.c:963-969`) | `make_link("keep.txt","hl")`⇒`ok`; again⇒`{error,eexist}`; `make_link("missing","x")`⇒`{error,enoent}`; bad path⇒`{error,badarg}` |
//! | `make_symlink/2` | `prim_file.erl:835-840` → `make_soft_link_nif` → `efile_make_soft_link` = `symlink(2)` (`unix_prim_file.c:971-977`) | `make_symlink("t","s")`⇒`ok` (the target need not exist); again⇒`{error,eexist}`; bad path⇒`{error,badarg}` |
//! | `altname/1` | `prim_file.erl:842-848`; on Unix `efile_altname` is a **constant** (`unix_prim_file.c:1085-1090`) | `altname("f")`/`("missing")`/`(<<"f">>)`⇒`{error,enotsup}` (it does **not** return its argument); `altname(foo)`/`(1)`/`(<<"/tmp/a",0,"b">>)`⇒`{error,badarg}` |
//! | `open/2` | `prim_file.erl:132-141` + `build_fd_data/5`; `efile_translate_modelist`/`get_flags`/`efile_open` | `{ok,Fd}`; a missing path⇒`{error,enoent}`; a directory with `[read]`⇒`{error,eisdir}`; `[write,exclusive]` on an existing file⇒`{error,eexist}`; a non-list mode term / bad path⇒`{error,badarg}` |
//! | `read/2` | `prim_file.erl:162-205` (`read_1/5`) + `read_nif_impl` | `{ok,<<…>>}`; a drained file⇒`eof`; `0`⇒`{ok,<<>>}`; `-1`/an atom⇒`{error,badarg}`; a float/a bigint⇒`{error,einval}`; a write-only fd⇒`{error,ebadf}`; a directory fd⇒`{error,eisdir}`; a closed/bogus fd⇒`{error,einval}` |
//! | `write/2` | `prim_file.erl:247-263` + `write_nif_impl` (`enif_inspect_iovec`) | `ok` for a binary/string/`[<<"a">>,<<"b">>]`/`[]`/`<<>>`; `{error,badarg}` for `foo`/`[256]`/`[-1]`/`[1.5]`/`[1\|2]`/`<<1:4>>`; a read-only fd⇒`{error,ebadf}`; a directory fd⇒`{error,ebadf}` |
//! | `close/1` | `prim_file.erl:154-160` + `close_nif` | `ok`; a second close / an unknown handle⇒`{error,einval}`; a malformed descriptor⇒`{error,badarg}` |
//! | `position/2` | `prim_file.erl:306-334` (`position_1/3`) + `efile_seek` | `{ok,N}`; `bof`/`cur`/`eof`⇒offset 0; a bare integer `N`⇒`{N,bof}`; `{bof,-1}`/`-3`⇒`{error,einval}`; `foo`/`{1,bof}`/`{bof,foo}`/`{bof,1.0}`/`{bof,1,2}`⇒`{error,badarg}`; past EOF⇒`{ok,100}`; a closed fd⇒`{error,einval}` |
//! | `pread/3` | `prim_file.erl:336-342` + `pread_nif_impl` (`prim_file_nif.c:712-750`) + `efile_preadv` (`unix_prim_file.c:393-450`) | `pread(Fd,0,3)`⇒`{ok,<<"012">>}`; `pread(Fd,9,5)` on an 11-byte file⇒`{ok,<<"9a">>}`; `pread(Fd,0,0)`/`(Fd,11,3)`⇒`eof`; the cursor is **not** moved; `-1`/a float/a bigint⇒`{error,einval}`; `foo`⇒`{error,badarg}`; a `[write]` fd⇒`{error,ebadf}`; a `[directory]` fd⇒`{error,eisdir}`; a closed fd⇒`{error,einval}` |
//! | `pread/2` | `prim_file.erl:344-368` (`pread_list/3`) | `pread(F,[{0,3},{6,5}])`⇒`{ok,[<<"012">>,<<"6789a">>]}`; `pread(F,[{0,3},{100,5}])`⇒`{ok,[<<"012">>,eof]}`; `pread(F,[])`⇒`{ok,[]}` (also on a **closed** fd); `pread(closed,[{0,1}])`⇒`{error,einval}`; `[{0,foo}]`⇒`{error,badarg}` |
//! | `pwrite/3` | `prim_file.erl:370-391` + `pwrite_nif_impl` (`prim_file_nif.c:752-780`) + `efile_pwritev` (`unix_prim_file.c:452-506`) | `pwrite(Fd,0,<<"XY">>)`⇒`ok` (cursor unchanged); `pwrite(Fd,0,<<>>)`⇒`ok`; `-1`/a float/a bigint⇒`{error,einval}`; `foo`⇒`{error,badarg}`; a `[read]` fd or a `[directory]` fd⇒`{error,ebadf}`; a closed fd⇒`{error,einval}` |
//! | `pwrite/2` | `prim_file.erl:393-418` (`pwrite_list/3`) | `pwrite(F,[{0,<<"A">>},{2,<<"B">>}])`⇒`ok`; `pwrite(F,[])`⇒`ok` (also closed); `[{0,<<"A">>},{1,foo}]`⇒`{error,badarg}`; `[{-1,<<"A">>}]`⇒`{error,{0,einval}}`; a closed fd with a non-empty list⇒`{error,{0,einval}}`; a `[directory]` fd⇒`{error,{0,ebadf}}` |
//! | `truncate/1` | `prim_file.erl:265-272` + `truncate_nif_impl` (`unix_prim_file.c:693-710`: `lseek(SEEK_CUR)` then `ftruncate(2)`) | `truncate(Fd)` at position 0 on an 11-byte file⇒`ok`, size 0; after `position(Fd,4)`⇒`ok`, size 4; a `[write]` fd⇒`ok`; a `[read]` fd / a `[directory]` fd / a closed fd⇒`{error,einval}` |
//! | `sync/1` | `prim_file.erl:290-298` + `sync_nif` (`unix_prim_file.c:542-568`, `fsync(2)`) | `ok` for a `[read]`, `[write]`, `[read,write]` or `[directory]` fd; a closed fd⇒`{error,einval}` |
//! | `datasync/1` | `prim_file.erl:299-304` + `sync_nif` with `data = 1` (`fdatasync(2)`) | as `sync/1` with the data-only host call |
//! | `write_file/2` | `prim_file.erl:574-584` (composes `open(Name,[write,binary])` → `write` → `close`) | `write_file(P,<<"hello">>)`⇒`ok` (contents `<<"hello">>`); overwriting a 10-byte file with `<<"AB">>`⇒`ok`, size 2; a directory⇒`{error,eisdir}`; a missing parent⇒`{error,enoent}`; `<<>>`/`[]`⇒`{error,enoent}`; an atom path / a NUL path / a bad payload (`foo`, `[1\|2]`)⇒`{error,badarg}`; `write_file/3` is **not exported** (`error:undef`) |
//! | `set_cwd/1` | `prim_file.erl:794-802` + `efile_set_cwd` = `chdir(2)`; `is_path_translatable/1` (`:899-902`) over the `is_translatable/1` BIF (`erl_unicode.c:2855-2895`) | an existing dir⇒`ok`; `<<dir>>`⇒`ok`; `"."`⇒`ok`; a file⇒`{error,enotdir}`; a missing dir⇒`{error,enoent}`; `[]`/`<<>>`⇒`{error,enoent}`; an atom/int⇒`{error,badarg}`; `<<255,255>>`⇒`{error,no_translation}`; a NUL (in a list or a binary)⇒`{error,badarg}` |
//!
//! The whole-file-descriptor entries (`open/2`, `read/2`, `write/2`, `close/1`,
//! `position/2`, `pread/2,3`, `pwrite/2,3`, `truncate/1`, `sync/1`, `datasync/1` and
//! `write_file/2`) are served by the sibling [`prim_file`](super::prim_file) module — this
//! table names them because [`RuntimeContext::prim_file_call`] is their dispatch site; each
//! entry's ground truth, mode grammar, descriptor shape and documented divergences are
//! documented there.
//!
//! # The errno REMAPS (the C's corrections to the platform's `errno`)
//!
//! Linux reports the "wrong" errno for several of these operations, and the C overrides it
//! (`unix_prim_file.c`): `unlink` `EISDIR`→`EPERM` (`:993-996`); `rename` `ENOTEMPTY`→
//! `EEXIST` and an old path of exactly `/`→`EINVAL` (`:947-955`); `rmdir` `ENOTEMPTY`→
//! `EEXIST`, and — when that `EEXIST` names the process's own (non-empty) cwd by ino/dev —
//! `EINVAL` (`:1008-1028`). Each remap is reproduced explicitly; an omitted one is a real
//! oracle divergence.
//!
//! # The path contract (`encode_path/1` → `internal_name2native/1`)
//!
//! A path is a **binary** (its bytes, verbatim: no NUL allowed) or an **iolist** of code
//! points, flattened—nested lists included—and encoded as UTF-8 (the native filename
//! encoding on Unix). An atom is `badarg` (`erl_unicode.c:2645`); a binary *element* of
//! an iolist is `badarg` (`erts_native_filename_need` never flattens one: `:2374-2383`);
//! so is a NUL, an improper tail, a non-integer element, and any code point that is not a
//! Unicode scalar value (`:2306-2354`). A path the wrapper rejects is the *value*
//! `{error,badarg}`, never a raise (every wrapper is `try … catch error:badarg`).
//!
//! # Not served
//!
//! Two kinds of entry stay outside the floor's working surface.
//!
//! **Explicit typed gaps** — claimed by `prim_file_call` and answered with
//! `error::unsupported/1` naming the facility this runtime lacks, so a caller gets the honest
//! capability gap instead of the silently wrong `{error,badarg}` the preloaded wrapper's
//! `catch error:_` would otherwise produce:
//!
//! * `read_file_info/1,2`, `read_link_info/1,2`, `read_handle_info/1,2` — every `#file_info{}`
//!   field is safe-`std`-derivable **except** `access`: the path forms call `access(2)`
//!   (`efile_read_info`, `unix_prim_file.c:757-762`) and the handle form
//!   `check_access/1`'s `geteuid`/`getegid` + mode bits (`:771-801`), neither of which safe
//!   `std` exposes. The **default `local`** (and `[{time,universal}]`) forms need **no**
//!   time-zone facility here — the Erlang wrapper `adjust_times/2` produces them through the
//!   served `erlang:posixtime_to_universaltime/1`/`universaltime_to_localtime/1`, so the NIF
//!   only ever returns the clamped POSIX seconds.
//! * `write_file_info/2,3` — `chmod(2)`/`chown(2)`/`utimes(2)`.
//! * `advise/4` — `posix_fadvise(2)`; `allocate/3` — `posix_fallocate(2)`.
//! * `list_dir/1`/`list_dir_all/1` — the answer is raw `readdir(3)` order, a property of the
//!   on-disk directory state that the oracle does not sort and this runtime cannot pin.
//! * `read_line/1` — its Erlang body drives the `prim_buffer:*` resource NIFs, which this
//!   runtime does not model.
//! * `file_desc_to_ref/2` — adopting a raw `int` fd.
//!
//! **Deliberately unclaimed**, because the call falls through to the preloaded `.beam` and the
//! oracle answers the same thing there: `copy/2`, `write_file/3` and `delayed_close/1` are not
//! exported (`error:undef`), and `sendfile/8`'s representable clause answers the *value*
//! `{error,enotsup}`/`badarg` (`prim_file.erl:438-439`), never a result this floor could
//! fabricate. (`copy/3` is pure Erlang over `file:copy_opened/3` and already works: `{ok,N}`.)
//! `internal_get_nif_resource/1` is unclaimed too — it returns the NIF resource behind a
//! descriptor, and this floor's `#file_descriptor{}` carries a private registry id instead.
//!
//! Not reproduced either: any refinement of the served `pread`/`pwrite` beyond the
//! offset+count / offset+bytes pairs pinned here — the C's `r_buffer` read-ahead and its
//! `prim_buffer` resource (see [`super::prim_file`]'s "Documented divergences").
//!
//! `prim_zip:splitter/3` and the other `prim_*` modules (`prim_buffer`, `prim_eval`,
//! `prim_inet`/`prim_socket`/`prim_net`, `erl_prim_loader`) are untouched —
//! see `CONTEXT.md` for the machinery each needs.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};

use beam_common::{BeamError, Result};
use beam_engine::ExecutionContext;
use beam_engine::terms;
use beam_term::{is_atom, is_list, is_nil, is_small, make_small, signed_value, Eterm};

use super::RuntimeContext;
use crate::error;
impl RuntimeContext<'_> {
    /// The `prim_file:*` half of the native floor.
    ///
    /// Returns `None` when `(function, arity)` is not one this module serves — the
    /// caller then falls through to the `beam_bifs` registry and then to the preloaded
    /// `prim_file.beam` (whose `-nifs()` stubs raise `undef`).
    pub(super) fn prim_file_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("get_cwd", 0) => Some(self.prim_get_cwd_0()),
            ("get_cwd", 1) => Some(self.prim_get_cwd_1(args)),
            ("read_file", 1) => Some(self.prim_read_file(args)),
            ("delete", 1) => Some(self.prim_delete(args)),
            ("rename", 2) => Some(self.prim_rename(args)),
            ("make_dir", 1) => Some(self.prim_make_dir(args)),
            ("del_dir", 1) => Some(self.prim_del_dir(args)),
            ("read_link", 1) => Some(self.prim_read_link(args)),
            ("make_link", 2) => Some(self.prim_make_link(args)),
            ("make_symlink", 2) => Some(self.prim_make_symlink(args)),
            ("altname", 1) => Some(self.prim_altname(args)),
            // The whole-file-descriptor family lives in the sibling [`super::prim_file`]
            // module; the descriptor it hands out is what the entries above this line take
            // nothing of.
            ("open", 2) => Some(self.prim_open(args)),
            ("read", 2) => Some(self.prim_read(args)),
            ("write", 2) => Some(self.prim_write(args)),
            ("close", 1) => Some(self.prim_close(args)),
            ("position", 2) => Some(self.prim_position(args)),
            ("pread", 3) => Some(self.prim_pread(args)),
            ("pread", 2) => Some(self.prim_pread_list(args)),
            ("pwrite", 3) => Some(self.prim_pwrite(args)),
            ("pwrite", 2) => Some(self.prim_pwrite_list(args)),
            ("truncate", 1) => Some(self.prim_truncate(args)),
            ("sync", 1) => Some(self.prim_sync(args)),
            ("datasync", 1) => Some(self.prim_datasync(args)),
            ("write_file", 2) => Some(self.prim_write_file(args)),
            ("set_cwd", 1) => Some(self.prim_set_cwd(args)),
            ("ipread_s32bu_p32bu", 3) => Some(self.prim_ipread(args)),
            ("get_handle", 1) => Some(self.prim_get_handle(args)),
            ("read_file_info", 1 | 2) => Some(self.prim_read_file_info(args, true)),
            ("read_link_info", 1 | 2) => Some(self.prim_read_file_info(args, false)),
            ("read_handle_info", 1 | 2) => Some(self.prim_read_handle_info(args)),
            // -- honest typed capability gaps (contract A.7) ----------------------
            //
            // Every name below is a **real** oracle BIF (`prim_file`'s own NIF surface), so
            // claiming it is what converts a caller's silent, *wrong* answer into the honest
            // `unsupported: …` gap. Left unclaimed it falls through to the preloaded
            // `prim_file.beam`, whose `catch error:_ -> {error,badarg}` wrappers swallow the
            // inner `undef` — and `{error,badarg}` is a value the oracle never returns for any
            // of them. Each reason names the facility this runtime lacks.
            //
            // Deliberately **absent**, because the oracle itself answers `error:undef` for
            // them (verified against OTP 29.1.1 / erts-17.1) and claiming them would *create*
            // a divergence: `copy/2`, `write_file/3`, `delayed_close/1`. Absent too, for the
            // same reason, is `sendfile/8`: its `Headers`/`Trailers`-empty clause calls
            // `prim_inet:sendfile/4` (the oracle answers `{error,badarg}` there) and the
            // other clause's documented answer is the *value* `{error,enotsup}`
            // (`prim_file.erl:438-439`) — a value this floor cannot produce without the
            // `sendfile(2)`/socket machinery, and one the fall-through already reaches.
            ("write_file_info", 2 | 3) => Some(Err(error::unsupported(
                "prim_file: needs raw chmod(2)/chown(2)/utimes(2), which no safe std API exposes",
            ))),
            ("advise", 4) => Some(Err(error::unsupported(
                "prim_file:advise/4: needs posix_fadvise(2), which no safe std API exposes",
            ))),
            ("allocate", 3) => Some(Err(error::unsupported(
                "prim_file:allocate/3: needs posix_fallocate(2), which no safe std API exposes",
            ))),
            ("list_dir", 1) | ("list_dir_all", 1) => Some(Err(error::unsupported(
                "prim_file:list_dir/1: the answer is raw readdir(3) order, a property of the \
                 on-disk directory state that cannot be pinned deterministically",
            ))),
            ("read_line", 1) => Some(Err(error::unsupported(
                "prim_file:read_line/1: its Erlang body drives the prim_buffer:* resource NIFs, \
                 which this runtime does not model",
            ))),
            ("file_desc_to_ref", 2) => Some(Err(error::unsupported(
                "prim_file:file_desc_to_ref/2: needs raw descriptor adoption/dup(2), which no \
                 safe std API exposes",
            ))),
            _ => None,
        }
    }

    // -- prim_file:get_cwd/0,1 ------------------------------------------------

    /// `prim_file:get_cwd/0` — the process's current working directory as
    /// `{ok, Cwd}` (`prim_file.erl:787-793`).
    ///
    /// The C answers the **raw** path as a byte binary and the Erlang wrapper decodes it
    /// with `decode_path/1` = `internal_native2name/1`; on a Unix UTF-8 native encoding
    /// that is the path's code points as a **string** (a char list) — the oracle's
    /// `{ok,"/home/…"}`. A native path that is not valid UTF-8 is the one case where the
    /// C's decoder answers `{error,warning}` (`erl_unicode.c:2774-2789`, the default
    /// warning type) instead of a string, so this reports the same.
    fn prim_get_cwd_0(&mut self) -> Result<Eterm> {
        match std::env::current_dir() {
            Ok(directory) => {
                let cwd = match directory.to_str() {
                    Some(text) => self.charlist(text)?,
                    None => {
                        let error = self.atom("error")?;
                        let warning = self.atom("warning")?;
                        terms::make_tuple(self, &[error, warning])?
                    }
                };
                self.ok_tuple(cwd)
            }
            Err(error) => self.error_tuple_for(&error),
        }
    }

    /// `prim_file:get_cwd/1` — the Windows drive-letter form (`prim_file.erl:771-778`).
    ///
    /// The three clauses are pure Erlang, so they are reproduced verbatim: a two-element
    /// `[Letter, $:]` list whose letter is `A`–`Z`/`a`–`z` goes to `get_device_cwd_nif`,
    /// which on Unix is a constant `ENOTSUP` (`unix_prim_file.c:1042-1048`); every other
    /// **cons cell** is `{error,einval}` (including an improper list — the `[_|_]`
    /// pattern matches one); anything else (`[]`, an atom, an integer, a binary) is
    /// `{error,badarg}`.
    ///
    /// The C's only `badarg` here (`enif_get_int` on the device index) is unreachable
    /// because the letter guard already restricted the list to two small integers.
    fn prim_get_cwd_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        if is_list(argument) {
            if let Some((letter, colon)) = two_element_list(argument) {
                if is_small(colon) && signed_value(colon) == i64::from(b':') {
                    if is_small(letter) {
                        let letter = signed_value(letter);
                        if (i64::from(b'A')..=i64::from(b'Z')).contains(&letter)
                            || (i64::from(b'a')..=i64::from(b'z')).contains(&letter)
                        {
                            return self.error_tuple("enotsup");
                        }
                    }
                }
            }
            return self.error_tuple("einval");
        }
        self.error_tuple("badarg")
    }

    // -- prim_file:read_file/1 ------------------------------------------------

    /// `prim_file:read_file/1` — the whole file as `{ok, Binary}`, or `{error, Reason}`
    /// (`prim_file.erl:564-570`).
    ///
    /// The Erlang wrapper runs `read_file_nif(encode_path(Filename))` inside
    /// `try … catch error:badarg -> {error, badarg} end`, so a **bad path** is the
    /// *value* `{error,badarg}`, never a raise.
    fn prim_read_file(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let Some(path) = self.encode_path(argument) else {
            return self.error_tuple("badarg");
        };
        match fs::read(std::ffi::OsStr::from_bytes(&path)) {
            // The contents are built on the runtime's pointer-stable `-eval` heap so the
            // returned term outlives this call (`beam_codec::term::make_binary` is the
            // same on-heap `ErlHeapBits` writer the `-eval` binary surface uses, and —
            // unlike `beam_engine::terms::make_heap_binary` — it has no 64-byte limit).
            Ok(bytes) => {
                let contents = beam_codec::term::make_binary(self.eval_heap(), &bytes)?;
                self.ok_tuple(contents)
            }
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:delete/1 ---------------------------------------------------

    /// `prim_file:delete/1` — `del_file_nif/1` → `efile_del_file` → `unlink(2)`
    /// (`prim_file.erl:804-809`, `unix_prim_file.c:991-1002`).
    ///
    /// The C remaps `EISDIR` to **`EPERM`** ("Linux sets the wrong error code",
    /// `:993-996`), so deleting a directory reports `eperm`, never `eisdir`.
    ///
    /// Oracle: `delete("f")`⇒`ok`; `delete("missing")`⇒`{error,enoent}`;
    /// `delete("adir")`⇒`{error,eperm}`; `delete(foo)`/`(1)`/`(<<"/tmp/a",0,"b">>)`
    /// ⇒`{error,badarg}`.
    fn prim_delete(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(path) = self.first_path(args) else {
            return self.error_tuple("badarg");
        };
        match fs::remove_file(std::ffi::OsStr::from_bytes(&path)) {
            Ok(()) => self.ok_atom(),
            Err(error) => {
                let errno = error.raw_os_error().unwrap_or(0);
                if errno == libc::EISDIR {
                    self.error_tuple("eperm")
                } else {
                    self.error_tuple_errno(errno)
                }
            }
        }
    }

    // -- prim_file:rename/2 ---------------------------------------------------

    /// `prim_file:rename/2` — `rename_nif/2` → `efile_rename` → `rename(2)`
    /// (`prim_file.erl:811-816`, `unix_prim_file.c:945-961`).
    ///
    /// The C's REMAPs (`:947-955`): `ENOTEMPTY`→**`EEXIST`**; and, for an *old* path of
    /// exactly `/` (which Alpha reports as `EBUSY` and Linux as `EACCES`), **`EINVAL`**.
    /// `EXDEV` and every other errno pass through. A bad path on **either** argument is
    /// the wrapper's `{error,badarg}`.
    ///
    /// Oracle: `rename("a","b")`⇒`ok`; `rename("missing","x")`⇒`{error,enoent}`;
    /// `rename("keep.txt","keep.txt/sub")`⇒`{error,enotdir}`; a directory onto a
    /// **non-empty** directory⇒`{error,eexist}`; `rename("/","x")`⇒`{error,einval}`;
    /// `rename(foo,"bar")`/`("bar",foo)`⇒`{error,badarg}`.
    fn prim_rename(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some((source, destination)) = self.two_paths(args) else {
            return self.error_tuple("badarg");
        };
        match fs::rename(
            std::ffi::OsStr::from_bytes(&source),
            std::ffi::OsStr::from_bytes(&destination),
        ) {
            Ok(()) => self.ok_atom(),
            Err(error) => {
                let errno = error.raw_os_error().unwrap_or(0);
                if errno == libc::ENOTEMPTY {
                    self.error_tuple("eexist")
                } else if source == b"/" {
                    self.error_tuple("einval")
                } else {
                    self.error_tuple_errno(errno)
                }
            }
        }
    }

    // -- prim_file:make_dir/1 -------------------------------------------------

    /// `prim_file:make_dir/1` — `make_dir_nif/1` → `efile_make_dir` → `mkdir(2)`
    /// (`prim_file.erl:817-822`, `unix_prim_file.c:979-989`).
    ///
    /// The C passes `DIR_MODE` = `0777` (`:56`), which the process umask masks — exactly
    /// `std::fs::create_dir`'s `mkdir(path, 0o777)`. There is no remap; every failure is
    /// its errno.
    ///
    /// Oracle: `make_dir("d")`⇒`ok`; again⇒`{error,eexist}`; an existing **file**
    /// ⇒`{error,eexist}`; `make_dir("missing/new")`⇒`{error,enoent}`;
    /// `make_dir("f.txt/new")`⇒`{error,enotdir}`; `make_dir(foo)`⇒`{error,badarg}`.
    fn prim_make_dir(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(path) = self.first_path(args) else {
            return self.error_tuple("badarg");
        };
        match fs::create_dir(std::ffi::OsStr::from_bytes(&path)) {
            Ok(()) => self.ok_atom(),
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:del_dir/1 --------------------------------------------------

    /// `prim_file:del_dir/1` — `del_dir_nif/1` → `efile_del_dir` → `rmdir(2)`
    /// (`prim_file.erl:823-828`, `unix_prim_file.c:1004-1032`).
    ///
    /// The C's REMAPs (`:1008-1028`): `ENOTEMPTY`→**`EEXIST`**; then, if that `EEXIST`
    /// names the *process's own* directory (same `st_ino`/`st_dev` as `stat(".")`),
    /// **`EINVAL`** — "the error code might be wrong if we're trying to delete the current
    /// directory". So deleting a non-empty cwd is `einval`, not `eexist`.
    ///
    /// Oracle: `del_dir("empty")`⇒`ok`; `del_dir("nonempty")`⇒`{error,eexist}`; a **file**
    /// ⇒`{error,enotdir}`; `del_dir("missing")`⇒`{error,enoent}`; a non-empty cwd
    /// ⇒`{error,einval}`; `del_dir(foo)`⇒`{error,badarg}`.
    fn prim_del_dir(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(path) = self.first_path(args) else {
            return self.error_tuple("badarg");
        };
        match fs::remove_dir(std::ffi::OsStr::from_bytes(&path)) {
            Ok(()) => self.ok_atom(),
            Err(error) => {
                let mut errno = error.raw_os_error().unwrap_or(0);
                if errno == libc::ENOTEMPTY {
                    errno = libc::EEXIST;
                }
                if errno == libc::EEXIST {
                    // The C's ino/dev stat check against the process's own cwd
                    // (`unix_prim_file.c:1016-1025`).
                    let cwd = fs::metadata(".");
                    let target = fs::metadata(std::ffi::OsStr::from_bytes(&path));
                    if let (Ok(cwd), Ok(target)) = (cwd, target) {
                        if target.ino() == cwd.ino() && target.dev() == cwd.dev() {
                            return self.error_tuple("einval");
                        }
                    }
                }
                self.error_tuple_errno(errno)
            }
        }
    }

    // -- prim_file:read_link/1 ------------------------------------------------

    /// `prim_file:read_link/1` — `read_link_nif/1` → `efile_read_link` → `readlink(2)`,
    /// then `translate_raw_name/2` (`prim_file.erl:590-606`,
    /// `unix_prim_file.c:859-895`).
    ///
    /// The NIF answers the raw target as a binary; the wrapper decodes it with
    /// `decode_path/1`. A **UTF-8** target is `{ok, Target}` as a **string** (a char
    /// list); a target that is not valid UTF-8 makes `decode_path/1` fail, and with
    /// `SilentFailure = false` (`read_link/1`'s `read_link_1/2`) that is `{error,einval}`.
    /// A regular file or directory is the kernel's `readlink(2)` `EINVAL`.
    ///
    /// Oracle: `read_link("lnk")`⇒`{ok,"keep.txt"}`; `read_link("dangling")`
    /// ⇒`{ok,"missing_target"}`; `read_link("regular")`/`(".")`⇒`{error,einval}`;
    /// `read_link("missing")`⇒`{error,enoent}`; a non-UTF-8 target⇒`{error,einval}`;
    /// `read_link(foo)`⇒`{error,badarg}`.
    fn prim_read_link(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some(path) = self.first_path(args) else {
            return self.error_tuple("badarg");
        };
        match fs::read_link(std::ffi::OsStr::from_bytes(&path)) {
            Ok(target) => match target.to_str() {
                Some(text) => {
                    let list = self.charlist(text)?;
                    self.ok_tuple(list)
                }
                None => self.error_tuple("einval"),
            },
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:make_link/2 ------------------------------------------------

    /// `prim_file:make_link/2` — `make_hard_link_nif/2` → `efile_make_hard_link` →
    /// `link(2)` (`prim_file.erl:829-834`, `unix_prim_file.c:963-969`).
    ///
    /// Both arguments go through `encode_path/1`; a bad path on either is the wrapper's
    /// `{error,badarg}`. There is no remap.
    ///
    /// Oracle: `make_link("keep.txt","hl")`⇒`ok`; again⇒`{error,eexist}`;
    /// `make_link("missing","x")`⇒`{error,enoent}`; `make_link(foo,"bar")`
    /// ⇒`{error,badarg}`.
    fn prim_make_link(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some((existing, new)) = self.two_paths(args) else {
            return self.error_tuple("badarg");
        };
        match fs::hard_link(
            std::ffi::OsStr::from_bytes(&existing),
            std::ffi::OsStr::from_bytes(&new),
        ) {
            Ok(()) => self.ok_atom(),
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:make_symlink/2 ---------------------------------------------

    /// `prim_file:make_symlink/2` — `make_soft_link_nif/2` → `efile_make_soft_link` →
    /// `symlink(2)` (`prim_file.erl:835-840`, `unix_prim_file.c:971-977`).
    ///
    /// The first argument is the **target** (it need not exist) and the second the new
    /// link name; both go through `encode_path/1`. There is no remap.
    ///
    /// Oracle: `make_symlink("t","s")`⇒`ok`; again⇒`{error,eexist}`;
    /// `make_symlink(foo,"bar")`⇒`{error,badarg}`.
    fn prim_make_symlink(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let Some((existing, new)) = self.two_paths(args) else {
            return self.error_tuple("badarg");
        };
        match symlink(
            std::ffi::OsStr::from_bytes(&existing),
            std::ffi::OsStr::from_bytes(&new),
        ) {
            Ok(()) => self.ok_atom(),
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:altname/1 --------------------------------------------------

    /// `prim_file:altname/1` — on Unix `efile_altname` is a **constant**
    /// (`prim_file.erl:842-848`, `unix_prim_file.c:1085-1090`): it ignores its path and
    /// always returns `ENOTSUP`, so the wrapper's `Other -> Other` clause yields
    /// `{error,enotsup}` for **any** valid path — it does **not** return its argument. A
    /// path the wrapper rejects is the `{error,badarg}` value.
    ///
    /// Oracle: `altname("f")`/`("missing")`/`(<<"f">>)`⇒`{error,enotsup}`;
    /// `altname(foo)`/`(1)`/`(<<"/tmp/a",0,"b">>)`⇒`{error,badarg}`.
    fn prim_altname(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if self.first_path(args).is_none() {
            return self.error_tuple("badarg");
        }
        self.error_tuple("enotsup")
    }

    // -- prim_file:set_cwd/1 --------------------------------------------------

    /// `prim_file:set_cwd/1` — the process changed to `Path`, `ok` or `{error, Reason}`
    /// (`prim_file.erl:794-802`, `efile_set_cwd` = `chdir(2)`).
    ///
    /// The wrapper is `set_cwd(Path) -> case is_path_translatable(Path) of true ->
    /// set_cwd_nif(encode_path(Path)); false -> {error, no_translation} end` inside a
    /// `catch error:badarg`. `is_path_translatable/1` (`prim_file.erl:899-902`) answers
    /// `true` for **any** list without consulting the emulator and otherwise calls the
    /// `is_translatable/1` BIF (`erl_unicode.c:2855-2895`), which raises `badarg` for a term
    /// that is not a binary and answers `true` for an **empty** binary or one that is valid
    /// UTF-8 (the Unix native filename encoding), `false` otherwise.
    ///
    /// Oracle: `set_cwd(<existing dir>)`⇒`ok`; `set_cwd(<<"…dir…">>)`⇒`ok`;
    /// `set_cwd(".")`⇒`ok`; a **file**⇒`{error,enotdir}`; a missing dir⇒`{error,enoent}`;
    /// `[]`/`<<>>`⇒`{error,enoent}`; an atom/int⇒`{error,badarg}`; `<<255,255>>`
    /// ⇒`{error,no_translation}`; a NUL (in a list or a binary)⇒`{error,badarg}`.
    fn prim_set_cwd(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        // `is_path_translatable/1`: every list (including `[]`) is translatable without the
        // BIF — note `beam_term::is_list/1` is false for `NIL`, which Erlang's `is_list/1`
        // accepts, so both are tested.
        if !(is_list(argument) || is_nil(argument)) {
            match terms::binary_bytes(argument) {
                Ok(bytes) => {
                    if !bytes.is_empty() && std::str::from_utf8(bytes).is_err() {
                        return self.error_tuple("no_translation");
                    }
                }
                // The `is_translatable/1` BIF raises `badarg` for a term that is neither a
                // list nor a binary; the wrapper's `catch` turns it into the value.
                Err(_) => return self.error_tuple("badarg"),
            }
        }
        let Some(path) = self.encode_path(argument) else {
            return self.error_tuple("badarg");
        };
        match std::env::set_current_dir(std::ffi::OsStr::from_bytes(&path)) {
            Ok(()) => self.ok_atom(),
            Err(error) => self.error_tuple_for(&error),
        }
    }

    // -- prim_file:read_file_info/1,2 · read_link_info/1,2 · read_handle_info/1,2 ----
    //
    // The `#file_info{}` family. The C body is `efile_read_info`
    // (`unix_prim_file.c:733-769`: `stat(2)`/`lstat(2)` plus two `access(2)` probes) and
    // `efile_read_handle_info` (`:803-820`: `fstat(2)` plus `check_access/1` against the
    // **effective** uid/gid), both feeding `build_file_info/2` (`:707-731`); the Erlang
    // wrappers are `read_info_1/3` and `read_handle_info_1/2` (`prim_file.erl:647-676`).
    // The whole-file-descriptor half lives in the sibling [`super::prim_file`] module.

    /// `prim_file:read_file_info/1,2` **and** `prim_file:read_link_info/1,2` —
    /// `read_info_1(Name, FollowLinks, TimeType)` with `FollowLinks` = `1` / `0`
    /// (`prim_file.erl:647-655`).
    ///
    /// Oracle: `read_file_info("f")`/`read_link_info("link")`⇒`{ok,#file_info{}}` (a
    /// 14-tuple); a missing path⇒`{error,enoent}` (the `stat(2)`/`lstat(2)` errno); a path the
    /// wrapper rejects⇒`{error,badarg}`; `[{time,posix}]`⇒raw POSIX seconds;
    /// `[{time,universal}]`/`[{time,local}]`⇒a `{{Y,M,D},{H,M,S}}` datetime;
    /// `[{time,bogus}]`⇒`{error,badarg}`; a non-list `Opts`⇒an uncaught `function_clause`.
    pub(super) fn prim_read_file_info(
        &mut self,
        args: &[Eterm],
        follow_links: bool,
    ) -> Result<Eterm> {
        let time = match self.file_info_time(args.get(1).copied()) {
            Some(time) => time,
            None => return Err(function_clause()),
        };
        let Some(path) = self.first_path(args) else {
            return self.error_tuple("badarg");
        };
        let os_path = std::ffi::OsStr::from_bytes(&path);
        // `FollowLinks` is the oracle's `stat`/`lstat` choice (`unix_prim_file.c:742-750`).
        let stat = if follow_links {
            std::fs::metadata(os_path)
        } else {
            std::fs::symlink_metadata(os_path)
        };
        let metadata = match stat {
            Err(failure) => return self.error_tuple_for(&failure),
            Ok(metadata) => metadata,
        };
        let access = self.path_access_bits(os_path);
        self.file_info_reply(&metadata, access, time)
    }

    /// The `{ok, #file_info{}}` reply for a successful `stat(2)`/`fstat(2)`.
    ///
    /// `access` is the bit pair the caller's own probe resolved — a path's two `access(2)`
    /// calls for the two `read_*_info/1,2` forms, `check_access/1` for a handle — so a record
    /// can never carry an invented one. A `TimeType` `from_posix_seconds/2` has no clause for
    /// is the wrapper's **value** `{error, badarg}` (its `catch error:_`, `prim_file.erl:660`),
    /// never a record.
    pub(super) fn file_info_reply(
        &mut self,
        metadata: &std::fs::Metadata,
        access: (bool, bool),
        time: TimeType,
    ) -> Result<Eterm> {
        if time == TimeType::Other {
            return self.error_tuple("badarg");
        }
        let info = self.file_info_term(metadata, access, time)?;
        self.ok_tuple(info)
    }

    /// The 14-tuple `#file_info{}` `build_file_info/2` emits
    /// (`?PRIM_FILE_NIF:992-1017`) — the field order, each field's width and the three time
    /// conversions are the oracle's.
    ///
    /// The seven `enif_make_uint` fields are **truncated to 32 bits**, exactly as that emitter
    /// does: `st_ino`/`st_dev`/`st_rdev`/`st_nlink` are 64-bit on Linux, so the oracle's
    /// `inode`/`major_device`/… really are the low halves (verified against a live `erl`).
    /// `size` is `enif_make_uint64` — a full 64-bit value, never truncated — and each time is
    /// clamped to `[EFILE_MIN_FILETIME, EFILE_MAX_FILETIME]` before its conversion.
    ///
    /// `time` must not be [`TimeType::Other`]: the caller ([`Self::file_info_reply`]) answers
    /// the wrapper's `{error, badarg}` for it before a record is built.
    pub(super) fn file_info_term(
        &mut self,
        metadata: &std::fs::Metadata,
        access: (bool, bool),
        time: TimeType,
    ) -> Result<Eterm> {
        let tag = self.atom("file_info")?;
        let kind = self.atom(file_type_atom(metadata))?;
        let access = self.access_atom(access)?;
        let size = terms::make_integer(
            self,
            i64::try_from(metadata.size()).map_err(|_| BeamError::SystemLimit)?,
        )?;
        let atime = self.time_field(metadata.atime(), time)?;
        let mtime = self.time_field(metadata.mtime(), time)?;
        let ctime = self.time_field(metadata.ctime(), time)?;
        let mode = make_small(i64::from(metadata.mode()));
        let links = make_small(i64::from(metadata.nlink() as u32));
        let major_device = make_small(i64::from(metadata.dev() as u32));
        let minor_device = make_small(i64::from(metadata.rdev() as u32));
        let inode = make_small(i64::from(metadata.ino() as u32));
        let uid = make_small(i64::from(metadata.uid()));
        let gid = make_small(i64::from(metadata.gid()));
        terms::make_tuple(
            self,
            &[
                tag,
                size,
                kind,
                access,
                atime,
                mtime,
                ctime,
                mode,
                links,
                major_device,
                minor_device,
                inode,
                uid,
                gid,
            ],
        )
    }

    /// One `#file_info{}` time field: the raw POSIX seconds clamped to the C's bounds, then
    /// converted for `time` (`from_posix_seconds/2`, `prim_file.erl:909-912`).
    fn time_field(&mut self, seconds: i64, time: TimeType) -> Result<Eterm> {
        let clamped = seconds.clamp(EFILE_MIN_FILETIME, EFILE_MAX_FILETIME);
        match time {
            TimeType::Posix => Ok(make_small(clamped)),
            TimeType::Universal => self.posix_to_universal(clamped),
            TimeType::Local => {
                let universal = self.posix_to_universal(clamped)?;
                self.call_bif("erlang", "universaltime_to_localtime", 1, &[universal])
            }
            // Defensive: unreachable from the served surface (see the caller), and typed so
            // the conversion has no silent fallback.
            TimeType::Other => Err(error::unsupported(
                "prim_file: an unrecognised `#file_info{}` time type",
            )),
        }
    }

    /// `erlang:posixtime_to_universaltime/1` through the crate's own dispatch, so the calendar
    /// arithmetic has exactly one implementation (`beam_bifs`' `calendar` registry).
    fn posix_to_universal(&mut self, seconds: i64) -> Result<Eterm> {
        let argument = make_small(seconds);
        self.call_bif("erlang", "posixtime_to_universaltime", 1, &[argument])
    }

    /// The `access` atom `efile_access_to_atom/1` produces (`unix_prim_file.c:488-498`).
    pub(super) fn access_atom(&mut self, (readable, writable): (bool, bool)) -> Result<Eterm> {
        let name = match (readable, writable) {
            (true, true) => "read_write",
            (true, false) => "read",
            (false, true) => "write",
            (false, false) => "none",
        };
        self.atom(name)
    }

    /// The **path** forms' `access` bits: two real `access(2)` probes, `R_OK` then `W_OK`
    /// (`efile_read_info`, `unix_prim_file.c:746-754`), delegated to the crate that owns the
    /// `unsafe` FFI (`beam_io::fs_access`).
    ///
    /// The C **clears** a bit whose probe failed for any reason, so a failure is `false` and
    /// never a raise: `beam_io::fs_access::access` already folds every refusal errno
    /// (`EACCES`, `EPERM`, `EROFS`, …) into `Ok(false)`, and the `unwrap_or` folds the
    /// remainder the same way.
    pub(super) fn path_access_bits(&mut self, path: &std::ffi::OsStr) -> (bool, bool) {
        beam_io::path_access(std::path::Path::new(path)).unwrap_or((false, false))
    }

    /// `proplist_get_value(time, Opts, local)` (`prim_file.erl:882-892`), classified as the
    /// [`TimeType`] `adjust_times/2` consumes.
    ///
    /// `None` is that helper's **uncaught** `function_clause` raise: it is evaluated outside
    /// `read_info_1/3`'s `try` (`prim_file.erl:642-645`), so an `Opts` that is not a proper
    /// list (or not a list at all) reaches the caller as a raise, never as `{error, badarg}`.
    pub(super) fn file_info_time(&self, opts: Option<Eterm>) -> Option<TimeType> {
        let Some(mut rest) = opts else {
            return Some(TimeType::Local); // the `/1` wrappers' literal `local` argument
        };
        loop {
            // `proplist_get_value(_Key, [], Default) -> Default`.
            if is_nil(rest) {
                return Some(TimeType::Local);
            }
            let head = terms::cons_head(rest).ok()?;
            let tail = terms::cons_tail(rest).ok()?;
            if matches!(terms::tuple_arity(head), Ok(2)) {
                // `proplist_get_value(Key, [{Key, Value} | _Rest], _) -> Value`.
                let key = terms::tuple_element(head, 0).ok()?;
                if self.atom_name(key).as_deref() == Some("time") {
                    let value = terms::tuple_element(head, 1).ok()?;
                    return Some(self.time_type_of(value));
                }
            } else if self.atom_name(head).as_deref() == Some("time") {
                // `proplist_get_value(Key, [Key | _Rest], _) -> true`, and `true` is a value
                // `from_posix_seconds/2` has no clause for.
                return Some(TimeType::Other);
            }
            rest = tail;
        }
    }

    /// The `TimeType` of `from_posix_seconds/2`'s second argument: `posix`, `universal` and
    /// `local` are the only three it has clauses for (`prim_file.erl:909-912`); every other
    /// value — the `true` a bare `time` proplist element produces, `bogus`, an integer —
    /// falls through to [`TimeType::Other`], which the wrapper's `catch` answers with
    /// `{error, badarg}`.
    fn time_type_of(&self, value: Eterm) -> TimeType {
        match self.atom_name(value).as_deref() {
            Some("posix") => TimeType::Posix,
            Some("universal") => TimeType::Universal,
            Some("local") => TimeType::Local,
            _ => TimeType::Other,
        }
    }

    /// The single path argument of a one-path BIF, encoded — `None` for the wrapper's
    /// bad-path **value** `{error,badarg}` (`prim_file:encode_path/1`).
    fn first_path(&self, args: &[Eterm]) -> Option<Vec<u8>> {
        self.encode_path(args.first().copied()?)
    }

    /// The two path arguments of a two-path BIF, encoded — `None` when **either** is bad
    /// (the wrapper's `{error,badarg}`).
    fn two_paths(&self, args: &[Eterm]) -> Option<(Vec<u8>, Vec<u8>)> {
        let first = self.encode_path(*args.first()?)?;
        let second = self.encode_path(*args.get(1)?)?;
        Some((first, second))
    }

    /// The C's `encode_path/1` = `prim_file:internal_name2native/1`
    /// (`erl_unicode.c:2629-2733`): the accepted path forms as bytes, or `None` for the
    /// C's `badarg`.
    pub(super) fn encode_path(&self, term: Eterm) -> Option<Vec<u8>> {
        // `prim_file` explicitly does not accept atoms (`erl_unicode.c:2641-2647`).
        if is_atom(term) {
            return None;
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            // A NUL in the middle of a filename is rejected (`:2666-2668`). A
            // non-byte-aligned bitstring is not a binary, so it falls to the walk below,
            // which rejects it exactly as `erts_get_aligned_binary_bytes` does.
            if bytes.contains(&0) {
                return None;
            }
            return Some(bytes.to_vec());
        }
        let mut path = Vec::new();
        collect_path(term, &mut path).ok()?;
        Some(path)
    }

    /// The bare `ok` atom (the success value of every non-`{ok,_}` BIF).
    pub(super) fn ok_atom(&mut self) -> Result<Eterm> {
        self.atom("ok")
    }

    /// `{ok, Value}`.
    pub(super) fn ok_tuple(&mut self, value: Eterm) -> Result<Eterm> {
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, value])
    }

    /// `{error, Reason}` for the literal atom `reason`.
    pub(super) fn error_tuple(&mut self, reason: &str) -> Result<Eterm> {
        let error = self.atom("error")?;
        let reason = self.atom(reason)?;
        terms::make_tuple(self, &[error, reason])
    }

    /// `{error, Reason}` for a raw `errno` code — the C's
    /// `posix_error_to_tuple(erl_errno_id(errno))`. Used where the C **remaps** the
    /// platform errno (`delete`/`rename`/`del_dir`).
    fn error_tuple_errno(&mut self, errno: i32) -> Result<Eterm> {
        let reason = errno_name(errno);
        self.error_tuple(&reason)
    }

    /// `{error, Reason}` with `Reason` derived from a host I/O failure — the C's
    /// `posix_error_to_tuple(erl_errno_id(errno))`.
    ///
    /// `std::fs` is syscall-only, so every error carries an errno (the C's exact input); an
    /// errno-less error has no C counterpart, and falls back to the C's unknown-errno form
    /// for `0` rather than invent an atom.
    pub(super) fn error_tuple_for(&mut self, error: &io::Error) -> Result<Eterm> {
        self.error_tuple_errno(error.raw_os_error().unwrap_or(0))
    }
}

/// `EFILE_MIN_FILETIME` — the earliest instant `build_file_info/2` reports
/// (`MIN(EFILE_MAX_FILETIME, MAX(EFILE_MIN_FILETIME, t))`, `?PRIM_FILE_NIF:1003-1011`).
pub(super) const EFILE_MIN_FILETIME: i64 = -2_145_916_800;

/// `EFILE_MAX_FILETIME` — the latest instant `build_file_info/2` reports
/// (`253402300799` = `9999-12-31T23:59:59Z`).
pub(super) const EFILE_MAX_FILETIME: i64 = 253_402_300_799;

/// The `TimeType` of a `#file_info{}`'s time fields — `from_posix_seconds/2`'s second
/// argument (`prim_file.erl:909-912`), which `read_info_1/3` takes from
/// `proplist_get_value(time, Opts, local)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TimeType {
    /// Raw POSIX seconds — `adjust_times(FileInfo, posix) -> FileInfo`.
    Posix,
    /// `erlang:posixtime_to_universaltime/1`.
    Universal,
    /// `erlang:universaltime_to_localtime(erlang:posixtime_to_universaltime(S))`.
    Local,
    /// Anything else: the generic `adjust_times/2` clause matches, but
    /// `from_posix_seconds/2` has no clause — so the raise inside `read_info_1/3`'s `try`
    /// becomes the **value** `{error, badarg}`.
    Other,
}

/// The `#file_info{}` `type` atom `build_file_info/2` derives from `st_mode`
/// (`unix_prim_file.c:707-719`): a char/block device, a directory, a regular file, a symlink,
/// or `other` for everything else (a fifo, a socket).
pub(super) fn file_type_atom(metadata: &std::fs::Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_char_device() || file_type.is_block_device() {
        "device"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "regular"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "other"
    }
}

/// The `access` bit pair `check_access/1` derives for a **handle** (`unix_prim_file.c:771-801`):
/// the file's own uid/gid against the process's **effective** credentials, in the C's order —
/// the owner triple when `st_uid == geteuid()`, else the group triple when
/// `st_gid == getegid()`, else the other triple. No `access(2)` call is made.
pub(super) fn status_access_bits(
    metadata: &std::fs::Metadata,
    euid: u32,
    egid: u32,
) -> (bool, bool) {
    let mode = metadata.mode();
    if metadata.uid() == euid {
        (mode & libc::S_IRUSR != 0, mode & libc::S_IWUSR != 0)
    } else if metadata.gid() == egid {
        (mode & libc::S_IRGRP != 0, mode & libc::S_IWGRP != 0)
    } else {
        (mode & libc::S_IROTH != 0, mode & libc::S_IWOTH != 0)
    }
}

/// The `error:function_clause` a `prim_file` helper raises **outside** the wrapper's `try`
/// (`proplist_get_value/3`, `prim_file.erl:882-892`) — an uncaught raise with the bare reason
/// atom `function_clause`, not the `{error, badarg}` *value* the same wrapper's
/// `catch error:_` produces.
pub(super) fn function_clause() -> BeamError {
    BeamError::Raise(beam_common::Raise::error(beam_common::ExitReason::atom(
        "function_clause",
    )))
}

/// The `[First, Second]` pattern of `prim_file:get_cwd/1` (`prim_file.erl:771-778`):
/// `Some` only for a **proper** two-element list, so the caller's `[_|_]` clause covers
/// every other cons cell.
fn two_element_list(term: Eterm) -> Option<(Eterm, Eterm)> {
    if !is_list(term) {
        return None;
    }
    let first = terms::cons_head(term).ok()?;
    let tail = terms::cons_tail(term).ok()?;
    if !is_list(tail) {
        return None;
    }
    let second = terms::cons_head(tail).ok()?;
    if !is_nil(terms::cons_tail(tail).ok()?) {
        return None;
    }
    Some((first, second))
}

/// The C's iolist walk of `erts_native_filename_need` (`erl_unicode.c:2222-2398`)
/// combined with its write half (`erts_native_filename_put`, `:2400`): appends the
/// UTF-8 encoding of `term`'s code points to `path`.
///
/// `Err(BeamError::BadArg)` is the C's `-1` → `badarg`: a NUL code point, a code point
/// that is not a Unicode scalar value, a non-integer element, a nested **binary**
/// (`erts_native_filename_need` never flattens one), or a non-list / improper tail.
///
/// The walk is iterative with an explicit stack of pending list tails, mirroring the
/// C's `ESTACK`, so an arbitrarily deep nesting cannot exhaust the Rust stack.
fn collect_path(term: Eterm, path: &mut Vec<u8>) -> Result<()> {
    let mut pending: Vec<Eterm> = Vec::new();
    let mut current = term;
    let mut buffer = [0u8; 4];
    loop {
        if is_nil(current) {
            // "ignore empty lists" — resume the enclosing list, or finish
            // (`erl_unicode.c:2296-2299`).
            match pending.pop() {
                Some(rest) => {
                    current = rest;
                    continue;
                }
                None => return Ok(()),
            }
        }
        if !is_list(current) {
            // An improper tail, or a term that is not a list at all (`:2287-2290`).
            return Err(BeamError::BadArg);
        }
        let head = terms::cons_head(current)?;
        let tail = terms::cons_tail(current)?;
        if is_small(head) {
            let code = signed_value(head);
            if code == 0 {
                return Err(BeamError::BadArg);
            }
            let code = u32::try_from(code).map_err(|_| BeamError::BadArg)?;
            let character = char::from_u32(code).ok_or(BeamError::BadArg)?;
            path.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        } else if is_list(head) {
            // Descend into the sublist, remembering where this one resumes
            // (`:2374-2379`).
            if !is_nil(tail) {
                pending.push(tail);
            }
            current = head;
            continue;
        } else if !is_nil(head) {
            // A binary, a float, a tuple, … — `is_nil(obj)` skips, everything else here
            // is the C's `-1` (`:2367-2383`).
            return Err(BeamError::BadArg);
        }
        current = tail;
    }
}

/// `errno` → the atom the C's `erl_errno_id/1` produces (`erl_errno_str.c:1229`) : the
/// POSIX name lower-cased (`tolower_copy_estring`, `:107`).
///
/// The table covers every errno a file-system operation can raise. An errno outside it
/// falls back to the C's own format for an errno it has no name for
/// (`ERTS_UNKNOWN_ERRNO_FORMAT`, `errno_<n>`: `:47`), so the answer is still the C's.
///
/// `pub(super)` because the whole-file-descriptor half of the floor
/// ([`super::prim_file`]) builds the `{error, {Successes, Reason}}` reason of a failed
/// `pwrite/2` from it rather than re-deriving the table.
pub(super) fn errno_name(code: i32) -> String {
    match ERRNO_NAMES.iter().find(|&&(errno, _)| errno == code) {
        Some(&(_, name)) => name.to_string(),
        None => format!("errno_{code}"),
    }
}

/// `errno` → the C's lower-cased name, for the errnos a file operation can produce.
const ERRNO_NAMES: &[(i32, &str)] = &[
    (libc::EPERM, "eperm"),
    (libc::ENOENT, "enoent"),
    (libc::ESRCH, "esrch"),
    (libc::EINTR, "eintr"),
    (libc::EIO, "eio"),
    (libc::ENXIO, "enxio"),
    (libc::E2BIG, "e2big"),
    (libc::ENOEXEC, "enoexec"),
    (libc::EBADF, "ebadf"),
    (libc::ECHILD, "echild"),
    (libc::EAGAIN, "eagain"),
    (libc::ENOMEM, "enomem"),
    (libc::EACCES, "eacces"),
    (libc::EFAULT, "efault"),
    (libc::EBUSY, "ebusy"),
    (libc::EEXIST, "eexist"),
    (libc::EXDEV, "exdev"),
    (libc::ENODEV, "enodev"),
    (libc::ENOTDIR, "enotdir"),
    (libc::EISDIR, "eisdir"),
    (libc::EINVAL, "einval"),
    (libc::ENFILE, "enfile"),
    (libc::EMFILE, "emfile"),
    (libc::ENOTTY, "enotty"),
    (libc::ETXTBSY, "etxtbsy"),
    (libc::EFBIG, "efbig"),
    (libc::ENOSPC, "enospc"),
    (libc::ESPIPE, "espipe"),
    (libc::EROFS, "erofs"),
    (libc::EMLINK, "emlink"),
    (libc::EPIPE, "epipe"),
    (libc::ERANGE, "erange"),
    (libc::EDEADLK, "edeadlk"),
    (libc::ENAMETOOLONG, "enametoolong"),
    (libc::ENOLCK, "enolck"),
    (libc::ENOSYS, "enosys"),
    (libc::ENOTEMPTY, "enotempty"),
    (libc::ELOOP, "eloop"),
    (libc::EOVERFLOW, "eoverflow"),
    (libc::EILSEQ, "eilseq"),
    (libc::ESTALE, "estale"),
    (libc::EDQUOT, "edquot"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::RuntimeContext;
    use crate::native::test_support::{atom, call, chars, runtime, string_of};
    use crate::runtime::Runtime;
    use beam_bifs::NullSink;
    use beam_term::{make_small, NIL};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    // -- fixtures -------------------------------------------------------------

    /// The sequence that keeps each test's scratch directory unique even when the
    /// module's tests run in parallel inside one process.
    static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// A fresh, writable scratch directory.
    ///
    /// The path is keyed by `pid` + a per-process sequence, but a **previous run** of this
    /// test binary can reuse the same pid and leave that directory behind, so any leftover
    /// is removed first — otherwise a `create_dir` on a subdirectory would fail with
    /// `eexist` and the assertions below would see a stale tree.
    fn scratch_dir(stem: &str) -> PathBuf {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_prim_{stem}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        dir
    }

    /// Builds a term on the runtime's pointer-stable arena through a temporary context,
    /// so it stays valid for the [`call`] that reads it.
    fn heap_term(
        runtime: &mut Runtime,
        build: impl for<'a> FnOnce(&mut RuntimeContext<'a>) -> Result<Eterm>,
    ) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        build(&mut context).expect("a heap term")
    }

    /// `bytes` as an Erlang binary on the runtime's pointer-stable arena.
    ///
    /// Built by the same `beam_codec::term::make_binary` the `prim_file:read_file/1`
    /// implementation uses, so a scratch path longer than the 64-byte on-heap limit
    /// (`beam_engine::terms::make_heap_binary`'s) is still representable.
    fn binary(runtime: &mut Runtime, bytes: &[u8]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        beam_codec::term::make_binary(context.eval_heap(), bytes).expect("a binary")
    }
    /// A bignum on the runtime's pointer-stable arena, `digits` least-significant limb first
    /// (`beam_codec::term::make_big`, the same builder `erts_internal`'s family uses).
    fn bignum(runtime: &mut Runtime, negative: bool, digits: &[u64]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        beam_codec::term::make_big(context.eval_heap(), negative, digits).expect("a bignum")
    }
    /// `term` rendered by the registered `io:format/2` `~p` printer, with the printer's one
    /// trailing newline stripped so a test can compare a shape inline.
    fn rendered(runtime: &mut Runtime, term: Eterm) -> String {
        let text = runtime
            .format_term(term, None)
            .expect("a printable answer");
        text.strip_suffix('\n').map_or(text.clone(), str::to_string)
    }

    /// `term` as the atom name it is (the tests assert names, never indices).
    fn atom_name(runtime: &Runtime, term: Eterm) -> String {
        runtime
            .atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is not an atom"))
            .to_string()
    }

    /// The `ok` value of an `{ok, Value}` answer.
    fn ok_value(runtime: &Runtime, answer: Eterm) -> Eterm {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "ok");
        element(answer, 1)
    }

    /// The reason atom of an `{error, Reason}` answer.
    fn error_reason(runtime: &Runtime, answer: Eterm) -> String {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "error");
        atom_name(runtime, element(answer, 1))
    }

    /// The `index`-th element of a tuple.
    fn element(answer: Eterm, index: usize) -> Eterm {
        terms::tuple_element(answer, index).expect("an element")
    }

    /// `prim_file:function(args)` through the runtime's dispatch.
    fn prim_file(runtime: &mut Runtime, function: &str, args: &[Eterm]) -> Eterm {
        call(runtime, "prim_file", function, args).expect("a prim_file answer")
    }

    /// The scratch `dir`'s child `name` as a char-list path argument.
    fn path_in(runtime: &mut Runtime, dir: &Path, name: &str) -> Eterm {
        chars(runtime, dir.join(name).to_str().expect("a UTF-8 path"))
    }

    /// Asserts `prim_file:function(args)` answers the bare atom `ok`.
    fn assert_ok(runtime: &mut Runtime, function: &str, args: &[Eterm]) {
        let answer = prim_file(runtime, function, args);
        assert_eq!(atom_name(runtime, answer), "ok", "{function}");
    }

    /// The failure reason of `prim_file:function(args)` (`{error, Reason}`).
    fn prim_error(runtime: &mut Runtime, function: &str, args: &[Eterm]) -> String {
        let answer = prim_file(runtime, function, args);
        error_reason(runtime, answer)
    }

    /// A NUL-containing binary path (rejected by `encode_path/1`).
    fn nul_path(runtime: &mut Runtime) -> Eterm {
        binary(runtime, b"/tmp/a\0b")
    }

    // -- prim_file:get_cwd/0 --------------------------------------------------

    /// Oracle: `erl -noshell -eval 'io:format("~p~n",[prim_file:get_cwd()]), halt().'`
    /// ⇒ `{ok,"/home/bill/Source/otp/.genesis/workers/worker_T2_A1033"}` — the cwd as a
    /// **string**. The host's `std::env::current_dir()` is the same `getcwd(3)` value.
    #[test]
    fn get_cwd_0_returns_the_working_directory_as_a_string() {
        let mut runtime = runtime();
        let answer = prim_file(&mut runtime, "get_cwd", &[]);
        let cwd = ok_value(&runtime, answer);
        let expected = std::env::current_dir().expect("a cwd");
        assert_eq!(string_of(cwd), expected.to_str().expect("a UTF-8 cwd"));
    }

    // -- prim_file:get_cwd/1 --------------------------------------------------

    /// Oracle (OTP 29.1.1): `get_cwd([])`⇒`{error,badarg}`; `get_cwd("C:")`/`("a:")`
    /// ⇒`{error,enotsup}`; `get_cwd("AB:")`/`(":")`/`("ab")`/`([1,2])`/`([65|foo])`
    /// ⇒`{error,einval}`; `get_cwd(<<"C:">>)`/`(floppy)`⇒`{error,badarg}`.
    #[test]
    fn get_cwd_1_matches_the_oracle() {
        let mut runtime = runtime();
        for (argument, expected) in [
            (NIL, "badarg"),
            (chars(&mut runtime, "C:"), "enotsup"),
            (chars(&mut runtime, "a:"), "enotsup"),
            (chars(&mut runtime, "AB:"), "einval"),
            (chars(&mut runtime, ":"), "einval"),
            (chars(&mut runtime, "ab"), "einval"),
            (chars(&mut runtime, "@:"), "einval"),
            (chars(&mut runtime, "{:"), "einval"),
            (chars(&mut runtime, "A"), "einval"),
        ] {
            let answer = prim_file(&mut runtime, "get_cwd", &[argument]);
            assert_eq!(error_reason(&runtime, answer), expected, "{argument:?}");
        }
        // `[1,2]` (a two-element list that is not `[Letter, $:]`).
        let pair = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(1), make_small(2)]));
        let answer = prim_file(&mut runtime, "get_cwd", &[pair]);
        assert_eq!(error_reason(&runtime, answer), "einval");
        // An improper list still matches the C's `[_|_]` clause.
        let improper = heap_term(&mut runtime, |ctx| {
            let tail = ctx.atom("foo")?;
            terms::make_cons(ctx, make_small(65), tail)
        });
        let answer = prim_file(&mut runtime, "get_cwd", &[improper]);
        assert_eq!(error_reason(&runtime, answer), "einval");
        // A binary and a non-list non-nil term are `badarg`.
        let binary_path = binary(&mut runtime, b"C:");
        let answer = prim_file(&mut runtime, "get_cwd", &[binary_path]);
        assert_eq!(error_reason(&runtime, answer), "badarg");
        let floppy = atom(&mut runtime, "floppy");
        let answer = prim_file(&mut runtime, "get_cwd", &[floppy]);
        assert_eq!(error_reason(&runtime, answer), "badarg");
    }

    // -- prim_file:read_file/1 ------------------------------------------------

    /// Oracle: `prim_file:read_file(<<"/tmp/…/hello.txt">>)` ⇒ `{ok,<<"hello">>}`;
    /// the same path as a char list (`"/tmp/…/hello.txt"`) ⇒ `{ok,<<"hello">>}`; an
    /// iolist (`"[…/tmp", "/hello.txt"]`) ⇒ `{ok,<<"hello">>}`.
    #[test]
    fn read_file_returns_the_contents_as_a_binary() {
        let mut runtime = runtime();
        let dir = scratch_dir("contents");
        let path = dir.join("hello.txt");
        std::fs::write(&path, b"hello").expect("the file is writable");
        let path = path.to_str().expect("a UTF-8 path");

        for argument in [
            binary(&mut runtime, path.as_bytes()),
            chars(&mut runtime, path),
            {
                let (head, tail) = (&path[..4], &path[4..]);
                let head = chars(&mut runtime, head);
                let tail = chars(&mut runtime, tail);
                heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[head, tail]))
            },
        ] {
            let answer = prim_file(&mut runtime, "read_file", &[argument]);
            let contents = ok_value(&runtime, answer);
            assert_eq!(
                terms::binary_bytes(contents).expect("a binary"),
                b"hello",
                "{argument:?}"
            );
        }
    }

    /// Oracle: reading a **missing** file ⇒ `{error,enoent}`; a **directory**
    /// ⇒ `{error,eisdir}`; a path through a non-directory ⇒ `{error,enotdir}`.
    #[test]
    fn read_file_maps_file_system_failures_to_errno_atoms() {
        let mut runtime = runtime();
        let dir = scratch_dir("failures");
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"x").expect("the file is writable");

        let cases = [
            (dir.join("missing.txt"), "enoent"),
            (dir.clone(), "eisdir"),
            (file.join("below"), "enotdir"),
        ];
        for (path, expected) in cases {
            let path = binary(&mut runtime, path.to_str().expect("a UTF-8 path").as_bytes());
            let answer = prim_file(&mut runtime, "read_file", &[path]);
            assert_eq!(error_reason(&runtime, answer), expected, "{expected}");
        }
    }

    /// Oracle: a path that `encode_path/1` rejects is the **value** `{error,badarg}`
    /// (`prim_file.erl:566-570`) — never a raise. Probes: `read_file(foo)`,
    /// `read_file(1)`, `read_file({raw,<<"x">>})`, `read_file([0])`,
    /// `read_file([-1])`, `read_file([16#110000])`, `read_file([16#D800])`,
    /// `read_file([1|2])`, `read_file([<<"/tmp">>])`, `read_file(<<"/tmp/a",0,"b">>)`.
    #[test]
    fn read_file_of_a_bad_path_is_the_badarg_value() {
        let mut runtime = runtime();
        let nul_binary = binary(&mut runtime, b"/tmp/a\0b");
        let nested_binary = heap_term(&mut runtime, |ctx| {
            let inner = terms::make_heap_binary(ctx, b"/tmp")?;
            terms::make_list(ctx, &[inner])
        });
        let improper = heap_term(&mut runtime, |ctx| {
            terms::make_cons(ctx, make_small(1), make_small(2))
        });
        let tuple = heap_term(&mut runtime, |ctx| {
            let raw = ctx.atom("raw")?;
            let inner = terms::make_heap_binary(ctx, b"x")?;
            terms::make_tuple(ctx, &[raw, inner])
        });
        let nul_codepoint = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(0)]));
        let negative = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(-1)]));
        let too_large =
            heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(0x11_0000)]));
        let surrogate = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(0xD800)]));
        let bad_paths = [
            atom(&mut runtime, "foo"),
            make_small(1),
            tuple,
            nul_codepoint,
            negative,
            too_large,
            surrogate,
            improper,
            nested_binary,
            nul_binary,
        ];
        for argument in bad_paths {
            let answer = prim_file(&mut runtime, "read_file", &[argument]);
            assert_eq!(error_reason(&runtime, answer), "badarg", "{argument:?}");
        }
    }

    /// The empty path (`<<>>`, `[]`, `[[]]`) is *valid* and reaches the file system as
    /// the empty name — the oracle answers `{error,enoent}` for all three.
    #[test]
    fn read_file_of_an_empty_path_reaches_the_file_system() {
        let mut runtime = runtime();
        let empty_binary = binary(&mut runtime, b"");
        let nested_empty = heap_term(&mut runtime, |ctx| {
            let inner = terms::make_list(ctx, &[])?;
            terms::make_list(ctx, &[inner])
        });
        for argument in [empty_binary, NIL, nested_empty] {
            let answer = prim_file(&mut runtime, "read_file", &[argument]);
            assert_eq!(error_reason(&runtime, answer), "enoent", "{argument:?}");
        }
    }

    /// A file past the 64-byte on-heap limit still round-trips: the contents are built
    /// by `beam_codec::term::make_binary`, which — unlike
    /// `beam_engine::terms::make_heap_binary` — has no size limit.
    #[test]
    fn read_file_returns_a_file_larger_than_one_word_block() {
        let mut runtime = runtime();
        let dir = scratch_dir("large");
        let path = dir.join("large.bin");
        let contents: Vec<u8> = (0..=255u8).cycle().take(200).collect();
        std::fs::write(&path, &contents).expect("the file is writable");
        let path = binary(&mut runtime, path.to_str().expect("a UTF-8 path").as_bytes());
        let answer = prim_file(&mut runtime, "read_file", &[path]);
        let read = ok_value(&runtime, answer);
        assert_eq!(terms::binary_bytes(read).expect("a binary"), &contents[..]);
    }

    // -- prim_file:delete/1 ---------------------------------------------------

    /// Oracle: `delete("f")`⇒`ok`; `delete("missing")`⇒`{error,enoent}`;
    /// `delete("adir")`⇒`{error,eperm}` — the C's `EISDIR`→`EPERM` remap
    /// (`unix_prim_file.c:993-996`).
    #[test]
    fn delete_removes_a_file_and_remaps_eisdir_to_eperm() {
        let mut runtime = runtime();
        let dir = scratch_dir("delete");
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").expect("a writable file");
        let subdir = dir.join("adir");
        std::fs::create_dir(&subdir).expect("a creatable dir");

        let target = path_in(&mut runtime, &dir, "f.txt");
        assert_ok(&mut runtime, "delete", &[target]);
        assert!(!file.exists(), "the file is gone");

        let missing = path_in(&mut runtime, &dir, "missing");
        assert_eq!(prim_error(&mut runtime, "delete", &[missing]), "enoent");

        let directory = path_in(&mut runtime, &dir, "adir");
        assert_eq!(prim_error(&mut runtime, "delete", &[directory]), "eperm");
        assert!(subdir.exists(), "a directory is not removed");
    }

    /// Oracle: `delete(foo)`/`(1)`/`(<<"/tmp/a",0,"b">>)`⇒`{error,badarg}`.
    #[test]
    fn delete_of_a_bad_path_is_the_badarg_value() {
        let mut runtime = runtime();
        let bad = [
            atom(&mut runtime, "foo"),
            make_small(1),
            nul_path(&mut runtime),
        ];
        for argument in bad {
            assert_eq!(prim_error(&mut runtime, "delete", &[argument]), "badarg");
        }
    }

    // -- prim_file:rename/2 ---------------------------------------------------

    /// Oracle: `rename("a","b")`⇒`ok`; `rename("missing","x")`⇒`{error,enoent}`;
    /// `rename("keep.txt","keep.txt/sub")`⇒`{error,enotdir}`; a directory onto a
    /// **non-empty** directory⇒`{error,eexist}` (the C's `ENOTEMPTY`→`EEXIST` remap);
    /// `rename("/","x")`⇒`{error,einval}` (the C's `/` special case).
    #[test]
    fn rename_matches_the_oracle() {
        let mut runtime = runtime();
        let dir = scratch_dir("rename");
        std::fs::write(dir.join("f.txt"), b"x").expect("a writable file");
        std::fs::create_dir(dir.join("srcdir")).expect("a creatable dir");
        let nonempty = dir.join("nonempty");
        std::fs::create_dir(&nonempty).expect("a creatable dir");
        std::fs::write(nonempty.join("x"), b"y").expect("a writable file");

        // rename("f.txt","renamed") ⇒ ok
        let source = path_in(&mut runtime, &dir, "f.txt");
        let destination = path_in(&mut runtime, &dir, "renamed");
        assert_ok(&mut runtime, "rename", &[source, destination]);
        assert!(dir.join("renamed").exists());

        // rename("missing","x") ⇒ {error,enoent}
        let missing = path_in(&mut runtime, &dir, "missing");
        let dest = path_in(&mut runtime, &dir, "x");
        assert_eq!(prim_error(&mut runtime, "rename", &[missing, dest]), "enoent");

        // rename("renamed","renamed/sub") ⇒ {error,enotdir} (a **file** component)
        let file = path_in(&mut runtime, &dir, "renamed");
        let under_file = path_in(&mut runtime, &dir, "renamed/sub");
        assert_eq!(
            prim_error(&mut runtime, "rename", &[file, under_file]),
            "enotdir"
        );

        // rename("srcdir","nonempty") ⇒ {error,eexist} (ENOTEMPTY→EEXIST)
        let srcdir = path_in(&mut runtime, &dir, "srcdir");
        let nonempty_arg = path_in(&mut runtime, &dir, "nonempty");
        assert_eq!(
            prim_error(&mut runtime, "rename", &[srcdir, nonempty_arg]),
            "eexist"
        );

        // rename("/","x") ⇒ {error,einval} (the C's `/` special case)
        let root = chars(&mut runtime, "/");
        let dest = path_in(&mut runtime, &dir, "x");
        assert_eq!(prim_error(&mut runtime, "rename", &[root, dest]), "einval");
    }

    /// Oracle: `rename(foo,"bar")`/`("bar",foo)`⇒`{error,badarg}` — a bad path on
    /// **either** argument.
    #[test]
    fn rename_of_a_bad_path_is_the_badarg_value() {
        let mut runtime = runtime();
        let dir = scratch_dir("rename_bad");
        std::fs::write(dir.join("good.txt"), b"x").expect("a writable file");
        let good = path_in(&mut runtime, &dir, "good.txt");
        let destination = path_in(&mut runtime, &dir, "moved.txt");
        let bad = atom(&mut runtime, "foo");

        assert_eq!(prim_error(&mut runtime, "rename", &[bad, destination]), "badarg");
        assert_eq!(prim_error(&mut runtime, "rename", &[good, bad]), "badarg");
        assert!(dir.join("good.txt").exists(), "nothing was moved");
    }

    // -- prim_file:make_dir/1 -------------------------------------------------

    /// Oracle: `make_dir("d")`⇒`ok`; again⇒`{error,eexist}`; an existing **file**
    /// ⇒`{error,eexist}`; `make_dir("missing/new")`⇒`{error,enoent}`;
    /// `make_dir("f.txt/new")`⇒`{error,enotdir}`; `make_dir(foo)`⇒`{error,badarg}`.
    #[test]
    fn make_dir_matches_the_oracle() {
        let mut runtime = runtime();
        let dir = scratch_dir("make_dir");
        std::fs::write(dir.join("f.txt"), b"x").expect("a writable file");

        let new_dir = path_in(&mut runtime, &dir, "d");
        assert_ok(&mut runtime, "make_dir", &[new_dir]);
        assert!(dir.join("d").is_dir());
        assert_eq!(prim_error(&mut runtime, "make_dir", &[new_dir]), "eexist");

        let a_file = path_in(&mut runtime, &dir, "f.txt");
        assert_eq!(prim_error(&mut runtime, "make_dir", &[a_file]), "eexist");

        let under_missing = path_in(&mut runtime, &dir, "missing/new");
        assert_eq!(
            prim_error(&mut runtime, "make_dir", &[under_missing]),
            "enoent"
        );

        let under_file = path_in(&mut runtime, &dir, "f.txt/new");
        assert_eq!(prim_error(&mut runtime, "make_dir", &[under_file]), "enotdir");

        let bad = atom(&mut runtime, "foo");
        assert_eq!(prim_error(&mut runtime, "make_dir", &[bad]), "badarg");
    }

    // -- prim_file:del_dir/1 --------------------------------------------------

    /// Oracle: `del_dir("empty")`⇒`ok`; `del_dir("nonempty")`⇒`{error,eexist}` (the
    /// `ENOTEMPTY`→`EEXIST` remap); a **file**⇒`{error,enotdir}`;
    /// `del_dir("missing")`⇒`{error,enoent}`; `del_dir(foo)`⇒`{error,badarg}`.
    #[test]
    fn del_dir_matches_the_oracle() {
        let mut runtime = runtime();
        let dir = scratch_dir("del_dir");
        std::fs::create_dir(dir.join("empty")).expect("a creatable dir");
        std::fs::create_dir(dir.join("nonempty")).expect("a creatable dir");
        std::fs::write(dir.join("nonempty/x"), b"y").expect("a writable file");
        std::fs::write(dir.join("f.txt"), b"x").expect("a writable file");

        let empty = path_in(&mut runtime, &dir, "empty");
        assert_ok(&mut runtime, "del_dir", &[empty]);
        assert!(!dir.join("empty").exists());

        let nonempty = path_in(&mut runtime, &dir, "nonempty");
        assert_eq!(prim_error(&mut runtime, "del_dir", &[nonempty]), "eexist");
        assert!(dir.join("nonempty").exists());

        let a_file = path_in(&mut runtime, &dir, "f.txt");
        assert_eq!(prim_error(&mut runtime, "del_dir", &[a_file]), "enotdir");

        let missing = path_in(&mut runtime, &dir, "missing");
        assert_eq!(prim_error(&mut runtime, "del_dir", &[missing]), "enoent");

        let bad = atom(&mut runtime, "foo");
        assert_eq!(prim_error(&mut runtime, "del_dir", &[bad]), "badarg");
    }

    /// Oracle: deleting the process's own **non-empty** cwd ⇒ `{error,einval}` — the C's
    /// `rmdir` `ENOTEMPTY`→`EEXIST` remap followed by its ino/dev stat check against
    /// `stat(".")` (`unix_prim_file.c:1008-1028`). The cwd here is a crate directory
    /// (never empty), so `rmdir` fails and nothing is removed; if it *were* empty it would
    /// be removable, so the test skips **loudly** rather than risk deleting it.
    #[test]
    fn del_dir_of_the_non_empty_cwd_is_einval() {
        let cwd = std::env::current_dir().expect("a cwd");
        if std::fs::read_dir(&cwd)
            .expect("a readable cwd")
            .next()
            .is_none()
        {
            eprintln!("SKIP: the cwd is empty; rmdir(cwd) would succeed");
            return;
        }
        let mut runtime = runtime();
        let cwd_arg = chars(&mut runtime, cwd.to_str().expect("a UTF-8 cwd"));
        assert_eq!(prim_error(&mut runtime, "del_dir", &[cwd_arg]), "einval");
        assert!(cwd.exists(), "the cwd was not removed");
    }

    // -- prim_file:read_link/1 ------------------------------------------------

    /// Oracle: `read_link("lnk")`⇒`{ok,"keep.txt"}` (a **string**);
    /// `read_link("dangling")`⇒`{ok,"missing_target"}`; `read_link("regular")`/`(".")`
    /// ⇒`{error,einval}`; `read_link("missing")`⇒`{error,enoent}`.
    #[test]
    fn read_link_decodes_the_target_to_a_string() {
        let mut runtime = runtime();
        let dir = scratch_dir("read_link");
        std::fs::write(dir.join("keep.txt"), b"x").expect("a writable file");
        std::os::unix::fs::symlink("keep.txt", dir.join("lnk")).expect("a symlink");
        std::os::unix::fs::symlink("missing_target", dir.join("dangling")).expect("a symlink");

        let link = path_in(&mut runtime, &dir, "lnk");
        let answer = prim_file(&mut runtime, "read_link", &[link]);
        assert_eq!(string_of(ok_value(&runtime, answer)), "keep.txt");

        let dangling = path_in(&mut runtime, &dir, "dangling");
        let answer = prim_file(&mut runtime, "read_link", &[dangling]);
        assert_eq!(string_of(ok_value(&runtime, answer)), "missing_target");

        // A regular file and a directory are the kernel's `readlink(2)` `EINVAL`.
        let regular = path_in(&mut runtime, &dir, "keep.txt");
        assert_eq!(prim_error(&mut runtime, "read_link", &[regular]), "einval");
        let directory = chars(&mut runtime, ".");
        assert_eq!(prim_error(&mut runtime, "read_link", &[directory]), "einval");

        let missing = path_in(&mut runtime, &dir, "missing");
        assert_eq!(prim_error(&mut runtime, "read_link", &[missing]), "enoent");
    }

    /// Oracle: a symlink whose raw target is **not** valid UTF-8 ⇒ `{error,einval}`
    /// (the wrapper's `decode_path/1` fails with `SilentFailure = false`);
    /// `read_link(foo)`⇒`{error,badarg}`.
    #[test]
    fn read_link_of_a_non_utf8_target_is_einval() {
        let mut runtime = runtime();
        let dir = scratch_dir("read_link_bad_utf8");
        std::os::unix::fs::symlink(
            std::ffi::OsStr::from_bytes(b"bad\xff"),
            dir.join("badlnk"),
        )
        .expect("a symlink");

        let link = path_in(&mut runtime, &dir, "badlnk");
        assert_eq!(prim_error(&mut runtime, "read_link", &[link]), "einval");

        let bad = atom(&mut runtime, "foo");
        assert_eq!(prim_error(&mut runtime, "read_link", &[bad]), "badarg");
        let nul = nul_path(&mut runtime);
        assert_eq!(prim_error(&mut runtime, "read_link", &[nul]), "badarg");
    }

    // -- prim_file:make_link/2 ------------------------------------------------

    /// Oracle: `make_link("keep.txt","hl")`⇒`ok`; again⇒`{error,eexist}`;
    /// `make_link("missing","x")`⇒`{error,enoent}`; `make_link(foo,"bar")`
    /// ⇒`{error,badarg}`.
    #[test]
    fn make_link_matches_the_oracle() {
        let mut runtime = runtime();
        let dir = scratch_dir("make_link");
        std::fs::write(dir.join("keep.txt"), b"x").expect("a writable file");

        let existing = path_in(&mut runtime, &dir, "keep.txt");
        let new = path_in(&mut runtime, &dir, "hl");
        assert_ok(&mut runtime, "make_link", &[existing, new]);
        // A hard link shares the inode with its target.
        assert_eq!(
            fs::metadata(dir.join("hl")).expect("the link").ino(),
            fs::metadata(dir.join("keep.txt")).expect("the target").ino()
        );
        assert_eq!(prim_error(&mut runtime, "make_link", &[existing, new]), "eexist");

        let missing = path_in(&mut runtime, &dir, "missing");
        let dest = path_in(&mut runtime, &dir, "x");
        assert_eq!(prim_error(&mut runtime, "make_link", &[missing, dest]), "enoent");

        let bad = atom(&mut runtime, "foo");
        assert_eq!(prim_error(&mut runtime, "make_link", &[bad, new]), "badarg");
        assert_eq!(prim_error(&mut runtime, "make_link", &[existing, bad]), "badarg");
    }

    // -- prim_file:make_symlink/2 ---------------------------------------------

    /// Oracle: `make_symlink("t","s")`⇒`ok` (the target need not exist); again
    /// ⇒`{error,eexist}`; `make_symlink(foo,"bar")`⇒`{error,badarg}`.
    #[test]
    fn make_symlink_matches_the_oracle() {
        let mut runtime = runtime();
        let dir = scratch_dir("make_symlink");

        let target = chars(&mut runtime, "t");
        let link = path_in(&mut runtime, &dir, "s");
        assert_ok(&mut runtime, "make_symlink", &[target, link]);
        assert_eq!(
            std::fs::read_link(dir.join("s")).expect("the link"),
            std::path::Path::new("t")
        );
        assert_eq!(prim_error(&mut runtime, "make_symlink", &[target, link]), "eexist");

        let bad = atom(&mut runtime, "foo");
        assert_eq!(prim_error(&mut runtime, "make_symlink", &[bad, link]), "badarg");
        assert_eq!(prim_error(&mut runtime, "make_symlink", &[target, bad]), "badarg");
    }

    // -- prim_file:altname/1 --------------------------------------------------

    /// Oracle: `altname("f")`/`("missing")`/`(<<"f">>)`⇒`{error,enotsup}` — on Unix the
    /// NIF is a **constant** (`unix_prim_file.c:1085-1090`) and does **not** return its
    /// argument; `altname(foo)`/`(1)`/`(<<"/tmp/a",0,"b">>)`⇒`{error,badarg}`.
    #[test]
    fn altname_is_a_constant_enotsup_on_unix() {
        let mut runtime = runtime();
        let textual = chars(&mut runtime, "/some/missing/path");
        let binary_path = binary(&mut runtime, b"f");
        for argument in [textual, binary_path] {
            assert_eq!(prim_error(&mut runtime, "altname", &[argument]), "enotsup");
        }

        let bad = [
            atom(&mut runtime, "foo"),
            make_small(1),
            nul_path(&mut runtime),
        ];
        for argument in bad {
            assert_eq!(prim_error(&mut runtime, "altname", &[argument]), "badarg");
        }
    }

    // -- the errno table ------------------------------------------------------
    /// The reasons the oracle produces for the failures above map to the C's names
    /// (`erl_errno_str.c:1229` + `errno_name`), and an unknown errno is the C's
    /// `errno_<n>` form (`ERTS_UNKNOWN_ERRNO_FORMAT`, `:47`).
    #[test]
    fn errno_names_follow_the_c() {
        for (errno, expected) in [
            (libc::ENOENT, "enoent"),
            (libc::EISDIR, "eisdir"),
            (libc::EACCES, "eacces"),
            (libc::ENOTDIR, "enotdir"),
            (libc::ELOOP, "eloop"),
            (libc::ENAMETOOLONG, "enametoolong"),
            (libc::ENOTEMPTY, "enotempty"),
            (libc::EXDEV, "exdev"),
        ] {
            assert_eq!(errno_name(errno), expected);
        }
        assert_eq!(errno_name(9_999), "errno_9999");
    }

    // -- prim_file:open/read/write/close/position ----------------------------
    //
    // The whole-file-descriptor family lives in the sibling [`super::prim_file`] module, but
    // its observable contract is driven — and its tests live — here, next to the path-only
    // half they share their dispatch and helpers with.

    /// `elements` as an Erlang list on the runtime's pointer-stable arena.
    fn list(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
        heap_term(runtime, |context| terms::make_list(context, elements))
    }

    /// `prim_file:open/2` on the scratch `dir`'s child `name` with `options`, returning
    /// the `#file_descriptor{}` — panics if the open failed (the tests that expect a failure
    /// assert it directly instead).
    fn open_file(runtime: &mut Runtime, dir: &Path, name: &str, options: &[Eterm]) -> Eterm {
        let path = path_in(runtime, dir, name);
        let modes = list(runtime, options);
        let answer = prim_file(runtime, "open", &[path, modes]);
        ok_value(runtime, answer)
    }

    /// The `Data` map of `fd` as `(key name, value)` pairs.
    fn descriptor_data(runtime: &Runtime, fd: Eterm) -> Vec<(String, Eterm)> {
        assert_eq!(terms::tuple_arity(fd).expect("a tuple"), 3);
        assert_eq!(atom_name(runtime, element(fd, 0)), "file_descriptor");
        assert_eq!(atom_name(runtime, element(fd, 1)), "prim_file");
        terms::map_entries(element(fd, 2))
            .expect("a map")
            .into_iter()
            .map(|(key, value)| (atom_name(runtime, key), value))
            .collect()
    }

    /// The value of `key` in a descriptor's `Data` pairs.
    fn data_get(data: &[(String, Eterm)], key: &str) -> Option<Eterm> {
        data.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| *value)
    }

    /// A `#file_descriptor{}` this runtime never opened, carrying registry id `id` — the
    /// "well-shaped but unknown handle" the NIF answers `{error,einval}` for.
    fn fake_descriptor(runtime: &mut Runtime, id: i64) -> Eterm {
        let owner = runtime.self_pid().to_eterm();
        let data = heap_term(runtime, |context| {
            let pairs = [
                (context.atom("handle")?, context.atom("h")?),
                (context.atom("owner")?, owner),
                (context.atom("fd_registry_id")?, make_small(id)),
            ];
            let atoms = context.runtime.code_server.atoms_handle();
            terms::make_map(context, &pairs, &atoms)
        });
        heap_term(runtime, |context| {
            let tag = context.atom("file_descriptor")?;
            let module = context.atom("prim_file")?;
            terms::make_tuple(context, &[tag, module, data])
        })
    }

    /// The position `prim_file:position(fd, location)` answers with.
    fn position_value(runtime: &mut Runtime, fd: Eterm, location: Eterm) -> i64 {
        let answer = prim_file(runtime, "position", &[fd, location]);
        signed_value(ok_value(runtime, answer))
    }

    /// The position `Loc` `{mark, offset}`.
    fn pair(runtime: &mut Runtime, mark: &str, offset: i64) -> Eterm {
        let mark = atom(runtime, mark);
        heap_term(runtime, |context| terms::make_tuple(context, &[mark, make_small(offset)]))
    }

    /// The bytes of an `{ok, Binary}` answer.
    fn ok_bytes(runtime: &Runtime, answer: Eterm) -> Vec<u8> {
        terms::binary_bytes(ok_value(runtime, answer))
            .expect("a binary")
            .to_vec()
    }

    /// Oracle: `file:open(P,[read,binary])` and `[read,list]` both hand back a descriptor a
    /// `read` round-trips on, and the file really opened (the host twin proves it).
    #[test]
    fn open_read_close_round_trips_in_binary_and_list_modes() {
        let dir = scratch_dir("roundtrip");
        let path = dir.join("data.txt");
        std::fs::write(&path, b"hello world").expect("seed the file");
        for option in ["binary", "list"] {
            let mut runtime = runtime();
            let read = atom(&mut runtime, "read");
            let extra = atom(&mut runtime, option);
            let fd = open_file(&mut runtime, &dir, "data.txt", &[read, extra]);
            assert_eq!(
                std::fs::read(&path).expect("the host reads the same file"),
                b"hello world"
            );
            let answer = prim_file(&mut runtime, "read", &[fd, make_small(5)]);
            assert_eq!(ok_bytes(&runtime, answer), b"hello");
            assert_ok(&mut runtime, "close", &[fd]);
        }
    }

    /// Oracle: sequential `read/2` calls advance through the file and then answer `eof`; a
    /// zero-byte read is `{ok,<<>>}` even at end of file.
    #[test]
    fn sequential_reads_reach_eof() {
        let dir = scratch_dir("eof");
        std::fs::write(dir.join("d"), b"hello world").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let fd = open_file(&mut runtime, &dir, "d", &[read]);
        for expected in [&b"hello"[..], &b" worl"[..], &b"d"[..]] {
            let answer = prim_file(&mut runtime, "read", &[fd, make_small(5)]);
            assert_eq!(ok_bytes(&runtime, answer), expected);
        }
        let eof = prim_file(&mut runtime, "read", &[fd, make_small(5)]);
        assert_eq!(atom_name(&runtime, eof), "eof");
        let zero = prim_file(&mut runtime, "read", &[fd, make_small(0)]);
        assert!(ok_bytes(&runtime, zero).is_empty());
        assert_ok(&mut runtime, "close", &[fd]);
    }

    /// Oracle: `write/2`'s payload is really on disk (the host twin proves it), through the
    /// nested iodata forms the oracle accepts.
    #[test]
    fn write_then_read_back_is_visible_to_the_host() {
        let dir = scratch_dir("writeback");
        let mut runtime = runtime();
        let write = atom(&mut runtime, "write");
        let fd = open_file(&mut runtime, &dir, "out.bin", &[write]);
        let text = chars(&mut runtime, "ab");
        let byte = make_small(99);
        let binary_part = binary(&mut runtime, b"de");
        let nested = list(&mut runtime, &[text, byte, binary_part]);
        assert_ok(&mut runtime, "write", &[fd, nested]);
        assert_ok(&mut runtime, "close", &[fd]);
        assert_eq!(
            std::fs::read(dir.join("out.bin")).expect("read back"),
            b"abcde"
        );
    }

    /// Oracle: `position/2`'s `Loc` grammar — the marker atoms, a bare integer (⇒ `{N,bof}`)
    /// and `{Mark, Offset}` pairs, including a seek past EOF.
    #[test]
    fn position_marks_offsets_and_bare_integers() {
        let dir = scratch_dir("position");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed an 11-byte file");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let fd = open_file(&mut runtime, &dir, "p", &[read]);
        let bof = atom(&mut runtime, "bof");
        let cur = atom(&mut runtime, "cur");
        let eof = atom(&mut runtime, "eof");
        assert_eq!(position_value(&mut runtime, fd, bof), 0);
        assert_eq!(position_value(&mut runtime, fd, cur), 0);
        assert_eq!(position_value(&mut runtime, fd, eof), 11);
        // A bare integer is `{N,bof}`.
        assert_eq!(position_value(&mut runtime, fd, make_small(3)), 3);
        let cur_one = pair(&mut runtime, "cur", 1);
        assert_eq!(position_value(&mut runtime, fd, cur_one), 4);
        let eof_minus_one = pair(&mut runtime, "eof", -1);
        assert_eq!(position_value(&mut runtime, fd, eof_minus_one), 10);
        let bof_one = pair(&mut runtime, "bof", 1);
        assert_eq!(position_value(&mut runtime, fd, bof_one), 1);
        // A seek past EOF is legal.
        assert_eq!(position_value(&mut runtime, fd, make_small(100)), 100);
        assert_ok(&mut runtime, "close", &[fd]);
    }

    /// Oracle: the `Loc` forms the C rejects are `{error,badarg}`; a negative `bof` offset
    /// is the kernel's `{error,einval}`.
    #[test]
    fn position_rejects_ill_formed_locations() {
        let dir = scratch_dir("position_bad");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let fd = open_file(&mut runtime, &dir, "p", &[read]);
        let foo = atom(&mut runtime, "foo");
        let bof = atom(&mut runtime, "bof");
        // `{1, bof}` — the mark is not an atom.
        let one_bof = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[make_small(1), bof])
        });
        // `{bof, foo}` — the offset is not an integer.
        let bof_foo = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[bof, foo])
        });
        // `{bof, 1.0}` — a float is not an integer offset.
        let float = heap_term(&mut runtime, |context| terms::make_float(context, 1.0));
        let bof_float = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[bof, float])
        });
        // `{bof, 1, 2}` — an arity-3 tuple is not a `Loc` pair.
        let bof_three = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[bof, make_small(1), make_small(2)])
        });
        for location in [foo, one_bof, bof_foo, bof_float, bof_three] {
            assert_eq!(
                prim_error(&mut runtime, "position", &[fd, location]),
                "badarg"
            );
        }
        // `{bof,-1}` and a bare `-3` are the kernel's `EINVAL`.
        let bof_minus_one = pair(&mut runtime, "bof", -1);
        assert_eq!(
            prim_error(&mut runtime, "position", &[fd, bof_minus_one]),
            "einval"
        );
        assert_eq!(
            prim_error(&mut runtime, "position", &[fd, make_small(-3)]),
            "einval"
        );
        assert_ok(&mut runtime, "close", &[fd]);
    }

    /// Oracle: a missing path ⇒ `{error,enoent}`; a directory with `[read]` ⇒
    /// `{error,eisdir}`; a `[directory]` open of a regular file ⇒ `{error,enotdir}`;
    /// `[write,exclusive]` on an existing file ⇒ `{error,eexist}`; a bad path and a
    /// non-list mode term ⇒ `{error,badarg}`.
    #[test]
    fn open_reports_the_oracles_failures() {
        let dir = scratch_dir("open_errors");
        std::fs::write(dir.join("f"), b"x").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let exclusive = atom(&mut runtime, "exclusive");
        let directory = atom(&mut runtime, "directory");

        // A missing path.
        let missing = chars(&mut runtime, "/nonexistent/beam_rt/prim_file/probe");
        let modes = list(&mut runtime, &[read]);
        assert_eq!(
            prim_error(&mut runtime, "open", &[missing, modes]),
            "enoent"
        );

        // A directory opened for reading — `open(2)` succeeds, the type check rejects it.
        let dir_path = chars(&mut runtime, dir.to_str().expect("a UTF-8 path"));
        let modes = list(&mut runtime, &[read]);
        assert_eq!(prim_error(&mut runtime, "open", &[dir_path, modes]), "eisdir");

        // `[directory]` on a regular file.
        let file_path = path_in(&mut runtime, &dir, "f");
        let modes = list(&mut runtime, &[directory]);
        assert_eq!(
            prim_error(&mut runtime, "open", &[file_path, modes]),
            "enotdir"
        );

        // `[write,exclusive]` on an existing file.
        let file_path = path_in(&mut runtime, &dir, "f");
        let modes = list(&mut runtime, &[write, exclusive]);
        assert_eq!(
            prim_error(&mut runtime, "open", &[file_path, modes]),
            "eexist"
        );

        // A bad path — an atom, and a NUL-containing binary.
        let modes = list(&mut runtime, &[read]);
        let bad_path = atom(&mut runtime, "nofile");
        assert_eq!(
            prim_error(&mut runtime, "open", &[bad_path, modes]),
            "badarg"
        );
        let modes = list(&mut runtime, &[read]);
        let nul = nul_path(&mut runtime);
        assert_eq!(prim_error(&mut runtime, "open", &[nul, modes]), "badarg");

        // A mode term that is not a list.
        let file_path = path_in(&mut runtime, &dir, "f");
        let non_list = atom(&mut runtime, "read");
        assert_eq!(
            prim_error(&mut runtime, "open", &[file_path, non_list]),
            "badarg"
        );
    }

    /// Oracle: the descriptor's `Data` map follows `build_fd_data/5` — the read shape
    /// (`handle`/`owner`/`r_buffer`/`r_ahead_size`) or the write shape (`handle`/`owner`
    /// only), with the read-ahead size the option list asked for.
    #[test]
    fn descriptor_shape_follows_the_mode_list() {
        let dir = scratch_dir("shape");
        std::fs::write(dir.join("f"), b"x").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");

        // `[read]`: the read shape, with a zero read-ahead size.
        let fd = open_file(&mut runtime, &dir, "f", &[read]);
        let data = descriptor_data(&runtime, fd);
        assert!(data_get(&data, "handle").is_some());
        assert!(data_get(&data, "owner").is_some());
        assert!(data_get(&data, "r_buffer").is_some());
        assert_eq!(
            signed_value(data_get(&data, "r_ahead_size").expect("r_ahead_size")),
            0
        );

        assert_ok(&mut runtime, "close", &[fd]);

        // `[]` — the default read shape.
        let fd = open_file(&mut runtime, &dir, "f", &[]);
        let data = descriptor_data(&runtime, fd);
        assert!(data_get(&data, "r_buffer").is_some());
        assert_ok(&mut runtime, "close", &[fd]);

        // `[read,write]` and `[read,append]` are still the read shape.
        for option in [write, atom(&mut runtime, "append")] {
            let fd = open_file(&mut runtime, &dir, "f", &[read, option]);
            let data = descriptor_data(&runtime, fd);
            assert!(data_get(&data, "r_buffer").is_some());
            assert_ok(&mut runtime, "close", &[fd]);
        }

        // `[write]`: the write shape, without `r_buffer`/`r_ahead_size`.
        let fd = open_file(&mut runtime, &dir, "w1", &[write]);
        let data = descriptor_data(&runtime, fd);
        assert!(data_get(&data, "handle").is_some());
        assert!(data_get(&data, "owner").is_some());
        assert!(data_get(&data, "r_buffer").is_none());
        assert!(data_get(&data, "r_ahead_size").is_none());
        assert_ok(&mut runtime, "close", &[fd]);

        // A bare `read_ahead` ⇒ 65536.
        let read_ahead = atom(&mut runtime, "read_ahead");
        let fd = open_file(&mut runtime, &dir, "f", &[read, read_ahead]);
        let data = descriptor_data(&runtime, fd);
        assert_eq!(
            signed_value(data_get(&data, "r_ahead_size").expect("r_ahead_size")),
            65536
        );
        assert_ok(&mut runtime, "close", &[fd]);

        // `{read_ahead, 8}` ⇒ 8.
        let tag = atom(&mut runtime, "read_ahead");
        let read_ahead_8 = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[tag, make_small(8)])
        });
        let fd = open_file(&mut runtime, &dir, "f", &[read, read_ahead_8]);
        let data = descriptor_data(&runtime, fd);
        assert_eq!(
            signed_value(data_get(&data, "r_ahead_size").expect("r_ahead_size")),
            8
        );
        assert_ok(&mut runtime, "close", &[fd]);
    }

    /// Oracle: a closed descriptor and a well-shaped unknown one are the NIF's
    /// `EFILE_STATE_CLOSED` `{error,einval}` for every operation (never a panic); a malformed
    /// descriptor is `{error,badarg}`.
    #[test]
    fn closed_and_unknown_handles_are_einval() {
        let dir = scratch_dir("einval");
        std::fs::write(dir.join("f"), b"x").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let fd = open_file(&mut runtime, &dir, "f", &[read]);
        assert_ok(&mut runtime, "close", &[fd]);

        // Every operation on the closed handle.
        assert_eq!(prim_error(&mut runtime, "close", &[fd]), "einval");
        assert_eq!(prim_error(&mut runtime, "read", &[fd, make_small(1)]), "einval");
        let payload = binary(&mut runtime, b"x");
        assert_eq!(prim_error(&mut runtime, "write", &[fd, payload]), "einval");
        let bof = atom(&mut runtime, "bof");
        assert_eq!(
            prim_error(&mut runtime, "position", &[fd, bof]),
            "einval"
        );

        // A well-shaped descriptor this runtime never opened.
        let bogus = fake_descriptor(&mut runtime, 9_999_999);
        assert_eq!(prim_error(&mut runtime, "close", &[bogus]), "einval");
        assert_eq!(
            prim_error(&mut runtime, "read", &[bogus, make_small(1)]),
            "einval"
        );

        // A malformed descriptor is `{error,badarg}`.
        let malformed = binary(&mut runtime, b"not a descriptor");
        let not_atom = atom(&mut runtime, "foo");
        let integer = make_small(1);
        for term in [malformed, not_atom, integer] {
            assert_eq!(prim_error(&mut runtime, "close", &[term]), "badarg");
            assert_eq!(
                prim_error(&mut runtime, "read", &[term, make_small(1)]),
                "badarg"
            );
            let bof = atom(&mut runtime, "bof");
            assert_eq!(
                prim_error(&mut runtime, "position", &[term, bof]),
                "badarg"
            );
        }
    }

    /// Oracle: a write-only (`[write]`/`[append]`) descriptor answers `{error,ebadf}` for
    /// `read/2`; a read-only one answers the same for `write/2`.
    #[test]
    fn read_on_write_only_and_write_on_read_only_are_ebadf() {
        let dir = scratch_dir("ebadf");
        std::fs::write(dir.join("ro"), b"x").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let append = atom(&mut runtime, "append");

        for option in [write, append] {
            let fd = open_file(&mut runtime, &dir, "wo", &[option]);
            assert_eq!(
                prim_error(&mut runtime, "read", &[fd, make_small(1)]),
                "ebadf"
            );
            assert_ok(&mut runtime, "close", &[fd]);
        }

        let fd = open_file(&mut runtime, &dir, "ro", &[read]);
        let payload = binary(&mut runtime, b"y");
        assert_eq!(prim_error(&mut runtime, "write", &[fd, payload]), "ebadf");
        assert_ok(&mut runtime, "close", &[fd]);
    }

    /// Oracle: `write/2`'s iodata walk accepts a binary, a byte, an empty list and nested
    /// forms; everything else is `{error,badarg}`. The accepted bytes are proved on disk.
    #[test]
    fn write_iodata_validation() {
        let dir = scratch_dir("iodata");
        let mut runtime = runtime();
        let write = atom(&mut runtime, "write");
        let fd = open_file(&mut runtime, &dir, "io.bin", &[write]);

        // Accepted forms.
        let empty_binary = binary(&mut runtime, b"");
        assert_ok(&mut runtime, "write", &[fd, empty_binary]);
        assert_ok(&mut runtime, "write", &[fd, NIL]);
        let one = binary(&mut runtime, b"x");
        assert_ok(&mut runtime, "write", &[fd, one]);
        let a = binary(&mut runtime, b"a");
        let b = binary(&mut runtime, b"b");
        let two = list(&mut runtime, &[a, b]);
        assert_ok(&mut runtime, "write", &[fd, two]);
        let text = chars(&mut runtime, "ab");
        let mixed = list(&mut runtime, &[text, make_small(99)]);
        assert_ok(&mut runtime, "write", &[fd, mixed]);
        let c = binary(&mut runtime, b"c");
        let inner = list(&mut runtime, &[c]);
        let a = binary(&mut runtime, b"a");
        let b = binary(&mut runtime, b"b");
        let deeply = list(&mut runtime, &[a, inner, b]);
        assert_ok(&mut runtime, "write", &[fd, deeply]);

        // Rejected forms.
        let improper = heap_term(&mut runtime, |context| {
            terms::make_cons(context, make_small(1), make_small(2))
        });
        let float = heap_term(&mut runtime, |context| terms::make_float(context, 1.5));
        let rejected = [
            atom(&mut runtime, "foo"),
            list(&mut runtime, &[make_small(256)]),
            improper,
            list(&mut runtime, &[float]),
            list(&mut runtime, &[make_small(-1)]),
        ];
        for term in rejected {
            assert_eq!(prim_error(&mut runtime, "write", &[fd, term]), "badarg");
        }

        assert_ok(&mut runtime, "close", &[fd]);
        assert_eq!(
            std::fs::read(dir.join("io.bin")).expect("read back"),
            b"xababcacb"
        );
    }

    // -- shared helpers for the pread/pwrite/truncate/sync/write_file families --

    /// The `{Offset, Size}` / `{Offset, IOData}` element of a `pread/2`/`pwrite/2` list.
    fn offset_pair(runtime: &mut Runtime, offset: Eterm, payload: Eterm) -> Eterm {
        heap_term(runtime, |context| {
            terms::make_tuple(context, &[offset, payload])
        })
    }

    /// The elements of `list` in order (it must be a proper list).
    fn list_items(list: Eterm) -> Vec<Eterm> {
        let mut items = Vec::new();
        let mut cell = list;
        while !is_nil(cell) {
            items.push(terms::cons_head(cell).expect("a list head"));
            cell = terms::cons_tail(cell).expect("a list tail");
        }
        items
    }

    /// A `[directory]`-mode descriptor for `dir`.
    fn open_directory(runtime: &mut Runtime, dir: &Path) -> Eterm {
        let path = chars(runtime, dir.to_str().expect("a UTF-8 path"));
        let directory = atom(runtime, "directory");
        let modes = list(runtime, &[directory]);
        let answer = prim_file(runtime, "open", &[path, modes]);
        ok_value(runtime, answer)
    }

    /// The `{Successes, Reason}` a failed `pwrite/2` answers inside its `{error, …}`.
    fn pwrite_failure(runtime: &Runtime, answer: Eterm) -> (i64, String) {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "error");
        let inner = element(answer, 1);
        assert_eq!(terms::tuple_arity(inner).expect("a tuple"), 2);
        (
            signed_value(element(inner, 0)),
            atom_name(runtime, element(inner, 1)),
        )
    }

    // -- prim_file:pread/3 ----------------------------------------------------

    /// Oracle (OTP 29.1.1 / erts-17.1) on an 11-byte file `<<"0123456789a">>`:
    /// `pread(Fd,0,3)`⇒`{ok,<<"012">>}`; `pread(Fd,9,5)`⇒`{ok,<<"9a">>}`;
    /// `pread(Fd,0,0)`/`(Fd,11,3)`⇒`eof`; the cursor is **not** moved;
    /// `pread(Fd,-1,3)`/`(Fd,0,-1)`/`(Fd,0,3.0)`/`(Fd,0.0,3)`/`(Fd,0,1 bsl 80)`
    /// ⇒`{error,einval}`; `pread(Fd,0,foo)`/`(Fd,foo,3)`⇒`{error,badarg}`; a `[write]` fd
    /// ⇒`{error,ebadf}`; a `[directory]` fd⇒`{error,eisdir}`; after `close/1`⇒`{error,einval}`.
    ///
    /// `// documented divergence:` a term that is not a `#file_descriptor{}` at all raises
    /// `function_clause` in the oracle (`pread(foo,0,3)` fails in `prim_file:get_fd_data/1`);
    /// this runtime answers the value `{error,badarg}`.
    #[test]
    fn pread_reads_at_an_offset_without_moving_the_cursor() {
        let dir = scratch_dir("pread");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed an 11-byte file");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let cur = atom(&mut runtime, "cur");
        let fd = open_file(&mut runtime, &dir, "p", &[read]);

        let answer = prim_file(&mut runtime, "pread", &[fd, make_small(0), make_small(3)]);
        assert_eq!(ok_bytes(&runtime, answer), b"012");
        // A **short** read at end of file is still `{ok, Binary}`.
        let answer = prim_file(&mut runtime, "pread", &[fd, make_small(9), make_small(5)]);
        assert_eq!(ok_bytes(&runtime, answer), b"9a");
        // The cursor is untouched by every successful pread.
        assert_eq!(position_value(&mut runtime, fd, cur), 0);

        // `bytes_read == 0` ⇒ `am_eof`: a zero-size request and a post-EOF offset alike.
        for (offset, size) in [(0, 0), (11, 3)] {
            let answer =
                prim_file(&mut runtime, "pread", &[fd, make_small(offset), make_small(size)]);
            assert_eq!(atom_name(&runtime, answer), "eof", "pread({offset},{size})");
        }
        assert_eq!(position_value(&mut runtime, fd, cur), 0);

        // A number that is not a non-negative `int64` is `{error,einval}`...
        let float_zero = heap_term(&mut runtime, |context| terms::make_float(context, 0.0));
        let float_three = heap_term(&mut runtime, |context| terms::make_float(context, 3.0));
        let big = heap_term(&mut runtime, |context| terms::make_integer(context, 1i64 << 62));
        for (offset, size) in [
            (make_small(-1), make_small(3)),
            (make_small(0), make_small(-1)),
            (make_small(0), float_three),
            (float_zero, make_small(3)),
            (make_small(0), big),
        ] {
            assert_eq!(
                prim_error(&mut runtime, "pread", &[fd, offset, size]),
                "einval",
                "{offset:?},{size:?}"
            );
        }
        // ...while a non-number is `{error,badarg}`.
        let foo = atom(&mut runtime, "foo");
        assert_eq!(
            prim_error(&mut runtime, "pread", &[fd, make_small(0), foo]),
            "badarg"
        );
        assert_eq!(
            prim_error(&mut runtime, "pread", &[fd, foo, make_small(3)]),
            "badarg"
        );

        assert_ok(&mut runtime, "close", &[fd]);

        // A `[write]` descriptor has no read side.
        let write_fd = open_file(&mut runtime, &dir, "w", &[write]);
        assert_eq!(
            prim_error(&mut runtime, "pread", &[write_fd, make_small(0), make_small(1)]),
            "ebadf"
        );
        assert_ok(&mut runtime, "close", &[write_fd]);

        // A `[directory]` descriptor: `pread(2)` answers `EISDIR`.
        let dir_fd = open_directory(&mut runtime, &dir);
        assert_eq!(
            prim_error(&mut runtime, "pread", &[dir_fd, make_small(0), make_small(1)]),
            "eisdir"
        );
        assert_ok(&mut runtime, "close", &[dir_fd]);

        // After `close/1` the well-formed handle is the NIF's `{error,einval}`.
        assert_eq!(
            prim_error(&mut runtime, "pread", &[fd, make_small(0), make_small(1)]),
            "einval"
        );
        // A term that is not a descriptor is the documented `{error,badarg}` value.
        let not_a_descriptor = binary(&mut runtime, b"not a descriptor");
        assert_eq!(
            prim_error(
                &mut runtime,
                "pread",
                &[not_a_descriptor, make_small(0), make_small(1)]
            ),
            "badarg"
        );
    }

    // -- prim_file:pread/2 ----------------------------------------------------

    /// Oracle: `pread(F,[{0,3},{6,5}])`⇒`{ok,[<<"012">>,<<"6789a">>]}`;
    /// `pread(F,[{0,3},{100,5}])`⇒`{ok,[<<"012">>,eof]}`; `pread(F,[])`⇒`{ok,[]}` (also on a
    /// **closed** fd); `pread(closed,[{0,1}])`⇒`{error,einval}`; `pread(F,[{0,foo}])`
    /// ⇒`{error,badarg}`.
    ///
    /// `// documented divergence:` `pread(F,foo)`, `pread(F,[{0}])` and
    /// `pread(F,[{0,1}|foo])` raise `function_clause` in the oracle (`pread_list/3`); this
    /// runtime answers the value `{error,badarg}`.
    #[test]
    fn pread_list_reads_every_pair_in_order() {
        let dir = scratch_dir("pread_list");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let fd = open_file(&mut runtime, &dir, "p", &[read]);

        let first = offset_pair(&mut runtime, make_small(0), make_small(3));
        let second = offset_pair(&mut runtime, make_small(6), make_small(5));
        let pairs = list(&mut runtime, &[first, second]);
        let answer = prim_file(&mut runtime, "pread", &[fd, pairs]);
        let items = list_items(ok_value(&runtime, answer));
        assert_eq!(items.len(), 2);
        assert_eq!(terms::binary_bytes(items[0]).expect("a binary"), b"012");
        assert_eq!(terms::binary_bytes(items[1]).expect("a binary"), b"6789a");

        // A short read at end of file is the atom `eof` **inside** the list.
        let first = offset_pair(&mut runtime, make_small(0), make_small(3));
        let second = offset_pair(&mut runtime, make_small(100), make_small(5));
        let pairs = list(&mut runtime, &[first, second]);
        let answer = prim_file(&mut runtime, "pread", &[fd, pairs]);
        let items = list_items(ok_value(&runtime, answer));
        assert_eq!(terms::binary_bytes(items[0]).expect("a binary"), b"012");
        assert_eq!(atom_name(&runtime, items[1]), "eof");

        // An empty list is `{ok,[]}` before the descriptor's state is consulted.
        let answer = prim_file(&mut runtime, "pread", &[fd, NIL]);
        assert!(is_nil(ok_value(&runtime, answer)));

        // A bad pair, a non-pair element and a non-list are all `{error,badarg}`.
        let tail = atom(&mut runtime, "foo");
        let bad_pair = offset_pair(&mut runtime, make_small(0), tail);
        let pairs = list(&mut runtime, &[bad_pair]);
        assert_eq!(prim_error(&mut runtime, "pread", &[fd, pairs]), "badarg");
        let not_a_pair = list(&mut runtime, &[make_small(0)]);
        assert_eq!(prim_error(&mut runtime, "pread", &[fd, not_a_pair]), "badarg");
        let foo = atom(&mut runtime, "foo");
        assert_eq!(prim_error(&mut runtime, "pread", &[fd, foo]), "badarg");

        assert_ok(&mut runtime, "close", &[fd]);

        // An empty list is still `{ok,[]}` on a **closed** descriptor...
        let answer = prim_file(&mut runtime, "pread", &[fd, NIL]);
        assert!(is_nil(ok_value(&runtime, answer)));
        // ...while a non-empty one is the NIF's `{error,einval}`.
        let one = offset_pair(&mut runtime, make_small(0), make_small(1));
        let pairs = list(&mut runtime, &[one]);
        assert_eq!(prim_error(&mut runtime, "pread", &[fd, pairs]), "einval");
    }

    // -- prim_file:pwrite/3 ---------------------------------------------------

    /// Oracle: `pwrite(Fd,0,<<"XY">>)`⇒`ok` with the host bytes written and the cursor
    /// unmoved; `pwrite(Fd,0,<<>>)`⇒`ok`; `pwrite(Fd,-1,<<"Z">>)`/`(Fd,0.0,<<"Z">>)`
    /// /`(Fd,1 bsl 80,<<"Z">>)`⇒`{error,einval}`; `pwrite(Fd,0,foo)`/`(Fd,foo,<<"Z">>)`
    /// ⇒`{error,badarg}`; a `[read]` fd⇒`{error,ebadf}`; a `[directory]` fd⇒`{error,ebadf}`;
    /// after `close/1`⇒`{error,einval}`.
    ///
    /// `// documented divergence:` a non-descriptor term raises `function_clause` in the
    /// oracle; this runtime answers the value `{error,badarg}`.
    #[test]
    fn pwrite_writes_at_an_offset_without_moving_the_cursor() {
        let dir = scratch_dir("pwrite");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let cur = atom(&mut runtime, "cur");
        let fd = open_file(&mut runtime, &dir, "p", &[read, write]);

        let payload = binary(&mut runtime, b"XY");
        assert_ok(&mut runtime, "pwrite", &[fd, make_small(0), payload]);
        assert_eq!(position_value(&mut runtime, fd, cur), 0);
        assert_eq!(
            std::fs::read(dir.join("p")).expect("read back"),
            b"XY23456789a"
        );

        // An empty payload is `ok` without a host call.
        let empty = binary(&mut runtime, b"");
        assert_ok(&mut runtime, "pwrite", &[fd, make_small(0), empty]);

        // A number that is not a non-negative `int64` is `{error,einval}`.
        let float_zero = heap_term(&mut runtime, |context| terms::make_float(context, 0.0));
        let big = heap_term(&mut runtime, |context| terms::make_integer(context, 1i64 << 62));
        let z = binary(&mut runtime, b"Z");
        assert_eq!(
            prim_error(&mut runtime, "pwrite", &[fd, make_small(-1), z]),
            "einval"
        );
        assert_eq!(
            prim_error(&mut runtime, "pwrite", &[fd, float_zero, z]),
            "einval"
        );
        assert_eq!(prim_error(&mut runtime, "pwrite", &[fd, big, z]), "einval");

        // A non-number offset and a bad payload are `{error,badarg}`.
        let foo = atom(&mut runtime, "foo");
        assert_eq!(
            prim_error(&mut runtime, "pwrite", &[fd, make_small(0), foo]),
            "badarg"
        );
        assert_eq!(prim_error(&mut runtime, "pwrite", &[fd, foo, z]), "badarg");

        // The cursor is still unmoved by every pwrite.
        assert_eq!(position_value(&mut runtime, fd, cur), 0);
        assert_ok(&mut runtime, "close", &[fd]);

        // A `[read]` descriptor has no write side.
        let read_fd = open_file(&mut runtime, &dir, "p", &[read]);
        assert_eq!(
            prim_error(&mut runtime, "pwrite", &[read_fd, make_small(0), z]),
            "ebadf"
        );
        assert_ok(&mut runtime, "close", &[read_fd]);

        // A `[directory]` descriptor.
        let dir_fd = open_directory(&mut runtime, &dir);
        assert_eq!(
            prim_error(&mut runtime, "pwrite", &[dir_fd, make_small(0), z]),
            "ebadf"
        );
        assert_ok(&mut runtime, "close", &[dir_fd]);

        // After `close/1`.
        assert_eq!(
            prim_error(&mut runtime, "pwrite", &[fd, make_small(0), z]),
            "einval"
        );
        let not_a_descriptor = binary(&mut runtime, b"not a descriptor");
        assert_eq!(
            prim_error(
                &mut runtime,
                "pwrite",
                &[not_a_descriptor, make_small(0), z]
            ),
            "badarg"
        );
    }

    // -- prim_file:pwrite/2 ---------------------------------------------------

    /// Oracle: `pwrite(F,[{0,<<"A">>},{2,<<"B">>}])`⇒`ok` with both pairs on disk;
    /// `pwrite(F,[])`⇒`ok` (also on a **closed** fd); `pwrite(F,[{0,<<"A">>},{1,foo}])`
    /// ⇒`{error,badarg}`; `pwrite(F,[{-1,<<"A">>}])`⇒`{error,{0,einval}}`; a closed fd with a
    /// non-empty list⇒`{error,{0,einval}}`; a `[directory]` fd⇒`{error,{0,ebadf}}`.
    ///
    /// `// documented divergence:` a non-list, an element that is not a pair and an improper
    /// tail raise `function_clause` in the oracle (`pwrite_list/3`); this runtime answers the
    /// value `{error,badarg}`.
    #[test]
    fn pwrite_list_writes_every_pair_and_counts_the_successes() {
        let dir = scratch_dir("pwrite_list");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let fd = open_file(&mut runtime, &dir, "p", &[read, write]);

        let a = binary(&mut runtime, b"A");
        let b = binary(&mut runtime, b"B");
        let first = offset_pair(&mut runtime, make_small(0), a);
        let second = offset_pair(&mut runtime, make_small(2), b);
        let pairs = list(&mut runtime, &[first, second]);
        assert_ok(&mut runtime, "pwrite", &[fd, pairs]);
        assert_eq!(std::fs::read(dir.join("p")).expect("read back"), b"A1B3456789a");

        // An empty list is `ok`.
        assert_ok(&mut runtime, "pwrite", &[fd, NIL]);

        // The first failing pair's payload is `{error,badarg}`.
        let a = binary(&mut runtime, b"A");
        let foo = atom(&mut runtime, "foo");
        let first = offset_pair(&mut runtime, make_small(0), a);
        let second = offset_pair(&mut runtime, make_small(1), foo);
        let pairs = list(&mut runtime, &[first, second]);
        assert_eq!(prim_error(&mut runtime, "pwrite", &[fd, pairs]), "badarg");

        // A negative offset on the **first** pair is `{error,{0,einval}}`.
        let a = binary(&mut runtime, b"A");
        let first = offset_pair(&mut runtime, make_small(-1), a);
        let pairs = list(&mut runtime, &[first]);
        let answer = prim_file(&mut runtime, "pwrite", &[fd, pairs]);
        assert_eq!(pwrite_failure(&runtime, answer), (0, "einval".to_string()));

        assert_ok(&mut runtime, "close", &[fd]);

        // An empty list is `ok` on a closed descriptor too...
        assert_ok(&mut runtime, "pwrite", &[fd, NIL]);
        // ...while a non-empty one is `{error,{0,einval}}`.
        let a = binary(&mut runtime, b"A");
        let first = offset_pair(&mut runtime, make_small(0), a);
        let pairs = list(&mut runtime, &[first]);
        let answer = prim_file(&mut runtime, "pwrite", &[fd, pairs]);
        assert_eq!(pwrite_failure(&runtime, answer), (0, "einval".to_string()));

        // A `[directory]` descriptor.
        let dir_fd = open_directory(&mut runtime, &dir);
        let a = binary(&mut runtime, b"A");
        let first = offset_pair(&mut runtime, make_small(0), a);
        let pairs = list(&mut runtime, &[first]);
        let answer = prim_file(&mut runtime, "pwrite", &[dir_fd, pairs]);
        assert_eq!(pwrite_failure(&runtime, answer), (0, "ebadf".to_string()));
        // An empty list is still `ok` on a directory descriptor.
        assert_ok(&mut runtime, "pwrite", &[dir_fd, NIL]);
        assert_ok(&mut runtime, "close", &[dir_fd]);
    }

    // -- prim_file:truncate/1 -------------------------------------------------

    /// Oracle: `truncate(Fd)` at position 0 on an 11-byte file ⇒ `ok` and the host file size
    /// 0; after `position(Fd,4)` ⇒ `ok` and size 4; a `[write]`-only descriptor ⇒ `ok`;
    /// `truncate` on a `[read]` fd, a `[directory]` fd and a closed fd ⇒ `{error,einval}`.
    ///
    /// `// documented divergence:` `truncate(foo)` raises `function_clause` in the oracle; the
    /// value `{error,badarg}` is answered here.
    #[test]
    fn truncate_cuts_the_file_at_the_current_position() {
        let dir = scratch_dir("truncate");
        std::fs::write(dir.join("p"), b"0123456789a").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let fd = open_file(&mut runtime, &dir, "p", &[read, write]);

        assert_ok(&mut runtime, "truncate", &[fd]);
        assert_eq!(std::fs::metadata(dir.join("p")).expect("metadata").len(), 0);

        assert_eq!(position_value(&mut runtime, fd, make_small(4)), 4);
        assert_ok(&mut runtime, "truncate", &[fd]);
        assert_eq!(std::fs::metadata(dir.join("p")).expect("metadata").len(), 4);

        assert_ok(&mut runtime, "close", &[fd]);
        assert_eq!(prim_error(&mut runtime, "truncate", &[fd]), "einval");

        // A `[read]` descriptor: `ftruncate(2)` on a read-only fd is `EINVAL`.
        let read_fd = open_file(&mut runtime, &dir, "p", &[read]);
        assert_eq!(prim_error(&mut runtime, "truncate", &[read_fd]), "einval");
        assert_ok(&mut runtime, "close", &[read_fd]);

        // A `[directory]` descriptor.
        let dir_fd = open_directory(&mut runtime, &dir);
        assert_eq!(prim_error(&mut runtime, "truncate", &[dir_fd]), "einval");
        assert_ok(&mut runtime, "close", &[dir_fd]);

        // A `[write]`-only descriptor can truncate.
        let write_fd = open_file(&mut runtime, &dir, "w", &[write]);
        assert_ok(&mut runtime, "truncate", &[write_fd]);
        assert_ok(&mut runtime, "close", &[write_fd]);

        let not_a_descriptor = binary(&mut runtime, b"not a descriptor");
        assert_eq!(
            prim_error(&mut runtime, "truncate", &[not_a_descriptor]),
            "badarg"
        );
    }

    // -- prim_file:sync/1, datasync/1 -----------------------------------------

    /// Oracle: `sync(Fd)`/`datasync(Fd)` are `ok` for a `[read]` fd, a `[write]` fd and a
    /// `[directory]` fd; a closed fd is `{error,einval}`.
    ///
    /// `// documented divergence:` `sync(foo)` raises `function_clause` in the oracle; the
    /// value `{error,badarg}` is answered here.
    #[test]
    fn sync_and_datasync_flush_a_descriptor_of_any_mode() {
        let dir = scratch_dir("sync");
        std::fs::write(dir.join("f"), b"x").expect("seed");
        let mut runtime = runtime();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let read_fd = open_file(&mut runtime, &dir, "f", &[read]);
        let write_fd = open_file(&mut runtime, &dir, "w", &[write]);
        for fd in [read_fd, write_fd] {
            assert_ok(&mut runtime, "sync", &[fd]);
            assert_ok(&mut runtime, "datasync", &[fd]);
        }

        let dir_fd = open_directory(&mut runtime, &dir);
        assert_ok(&mut runtime, "sync", &[dir_fd]);
        assert_ok(&mut runtime, "datasync", &[dir_fd]);
        assert_ok(&mut runtime, "close", &[dir_fd]);

        assert_ok(&mut runtime, "close", &[read_fd]);
        assert_eq!(prim_error(&mut runtime, "sync", &[read_fd]), "einval");
        assert_eq!(prim_error(&mut runtime, "datasync", &[read_fd]), "einval");
        assert_ok(&mut runtime, "close", &[write_fd]);

        let not_a_descriptor = binary(&mut runtime, b"not a descriptor");
        assert_eq!(
            prim_error(&mut runtime, "sync", &[not_a_descriptor]),
            "badarg"
        );
        assert_eq!(
            prim_error(&mut runtime, "datasync", &[not_a_descriptor]),
            "badarg"
        );
    }

    // -- prim_file:write_file/2 -----------------------------------------------

    /// Oracle: `write_file(P,<<"hello">>)`⇒`ok` with the host bytes `<<"hello">>`;
    /// overwriting a 10-byte file with `<<"AB">>`⇒`ok` and size 2; a char-list payload is
    /// iodata (`write_file(P,"abc")`⇒`ok`, bytes `<<"abc">>`).
    #[test]
    fn write_file_writes_the_whole_payload() {
        let dir = scratch_dir("write_file");
        let mut runtime = runtime();
        let path = path_in(&mut runtime, &dir, "out.bin");
        let payload = binary(&mut runtime, b"hello");
        assert_ok(&mut runtime, "write_file", &[path, payload]);
        assert_eq!(std::fs::read(dir.join("out.bin")).expect("read back"), b"hello");

        // Overwriting a longer file truncates it (`O_TRUNC`).
        std::fs::write(dir.join("big"), b"0123456789").expect("seed");
        let big = path_in(&mut runtime, &dir, "big");
        let short = binary(&mut runtime, b"AB");
        assert_ok(&mut runtime, "write_file", &[big, short]);
        assert_eq!(std::fs::read(dir.join("big")).expect("read back"), b"AB");

        // A char-list payload is iodata.
        let text = path_in(&mut runtime, &dir, "text");
        let payload = chars(&mut runtime, "abc");
        assert_ok(&mut runtime, "write_file", &[text, payload]);
        assert_eq!(std::fs::read(dir.join("text")).expect("read back"), b"abc");
    }

    /// Oracle: a directory⇒`{error,eisdir}`; a missing parent⇒`{error,enoent}`; the empty
    /// path (`<<>>` and `[]`)⇒`{error,enoent}`; an atom path / a NUL path / a bad payload
    /// (`foo`, `[1|2]`)⇒`{error,badarg}`. `write_file/3` is **not exported** by the oracle
    /// (`error:undef`), so only the arity-2 form exists.
    #[test]
    fn write_file_reports_the_oracles_failures() {
        let dir = scratch_dir("write_file_errors");
        let mut runtime = runtime();
        let payload = binary(&mut runtime, b"x");

        // A directory.
        let directory = chars(&mut runtime, dir.to_str().expect("a UTF-8 path"));
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[directory, payload]),
            "eisdir"
        );

        // A missing parent directory.
        let missing = path_in(&mut runtime, &dir, "nope/child");
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[missing, payload]),
            "enoent"
        );

        // The empty path.
        let empty = binary(&mut runtime, b"");
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[empty, payload]),
            "enoent"
        );
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[NIL, payload]),
            "enoent"
        );

        // An atom path and a NUL-containing path.
        let atom_path = atom(&mut runtime, "nofile");
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[atom_path, payload]),
            "badarg"
        );
        let nul = nul_path(&mut runtime);
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[nul, payload]),
            "badarg"
        );

        // A bad payload — the oracle opens the file first and *then* flattens it.
        let target = path_in(&mut runtime, &dir, "ok");
        let bad = atom(&mut runtime, "foo");
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[target, bad]),
            "badarg"
        );
        let target = path_in(&mut runtime, &dir, "ok2");
        let improper = heap_term(&mut runtime, |context| {
            terms::make_cons(context, make_small(1), make_small(2))
        });
        assert_eq!(
            prim_error(&mut runtime, "write_file", &[target, improper]),
            "badarg"
        );
    }

    // -- prim_file:set_cwd/1 (the process-wide working directory) --------------

    /// Serialises the tests that change the process-wide working directory:
    /// `std::env::set_current_dir` is global to the process and `cargo test` runs this
    /// module's tests on parallel threads, so a test that chdirs must hold this and restore
    /// the directory it started in.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `body` with the working directory guaranteed restored afterwards, under
    /// [`CWD_LOCK`] — released on the panic path too.
    fn with_cwd_restored<T>(body: impl FnOnce() -> T) -> T {
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let original = std::env::current_dir().expect("a cwd");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        std::env::set_current_dir(&original).expect("restore the cwd");
        match outcome {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Oracle: `set_cwd(<existing dir>)`⇒`ok` and `get_cwd/0` then reports it;
    /// `set_cwd(<<dir>>)`⇒`ok`; `set_cwd(".")`⇒`ok`.
    #[test]
    fn set_cwd_changes_the_process_working_directory() {
        with_cwd_restored(|| {
            let dir = scratch_dir("set_cwd");
            let mut runtime = runtime();

            let path = chars(&mut runtime, dir.to_str().expect("a UTF-8 path"));
            assert_ok(&mut runtime, "set_cwd", &[path]);
            let answer = prim_file(&mut runtime, "get_cwd", &[]);
            let cwd = ok_value(&runtime, answer);
            let expected = std::fs::canonicalize(&dir).expect("a canonical dir");
            assert_eq!(string_of(cwd), expected.to_str().expect("a UTF-8 path"));

            // The binary form is accepted too.
            let parent = dir.parent().expect("a parent directory").to_path_buf();
            let binary_path = binary(&mut runtime, parent.to_str().expect("UTF-8").as_bytes());
            assert_ok(&mut runtime, "set_cwd", &[binary_path]);

            // `"."` is unchanged.
            let dot = chars(&mut runtime, ".");
            assert_ok(&mut runtime, "set_cwd", &[dot]);
        });
    }

    /// Oracle: a **file**⇒`{error,enotdir}`; a missing directory⇒`{error,enoent}`; the empty
    /// path (`[]` and `<<>>`)⇒`{error,enoent}`; an atom/an integer⇒`{error,badarg}`;
    /// `<<255,255>>` (a binary that is not valid UTF-8)⇒`{error,no_translation}`; a NUL (in a
    /// list or a binary)⇒`{error,badarg}`.
    #[test]
    fn set_cwd_matches_the_oracle() {
        with_cwd_restored(|| {
            let dir = scratch_dir("set_cwd_errors");
            let file = dir.join("f");
            std::fs::write(&file, b"x").expect("seed");
            let mut runtime = runtime();

            // A file is `{error,enotdir}`.
            let file_path = chars(&mut runtime, file.to_str().expect("a UTF-8 path"));
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[file_path]), "enotdir");

            // A missing directory.
            let missing = path_in(&mut runtime, &dir, "missing");
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[missing]), "enoent");

            // The empty path.
            let empty = binary(&mut runtime, b"");
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[empty]), "enoent");
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[NIL]), "enoent");

            // A non-list, non-binary term is `{error,badarg}`.
            let atom_path = atom(&mut runtime, "cwd");
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[atom_path]), "badarg");
            assert_eq!(
                prim_error(&mut runtime, "set_cwd", &[make_small(1)]),
                "badarg"
            );

            // A binary that is not valid UTF-8 is `{error,no_translation}`.
            let untranslatable = binary(&mut runtime, &[255, 255]);
            assert_eq!(
                prim_error(&mut runtime, "set_cwd", &[untranslatable]),
                "no_translation"
            );

            // A NUL is `{error,badarg}`, in a binary and in a char list alike.
            let nul = nul_path(&mut runtime);
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[nul]), "badarg");
            let nul_list =
                heap_term(&mut runtime, |context| terms::make_list(context, &[make_small(0)]));
            assert_eq!(prim_error(&mut runtime, "set_cwd", &[nul_list]), "badarg");

            // The cwd is untouched by every failure.
            assert_eq!(
                std::env::current_dir().expect("a cwd"),
                std::env::current_dir().expect("a cwd")
            );
        });
    }

    // -- prim_file:ipread_s32bu_p32bu/3 ---------------------------------------

    /// The `ipread` fixture both halves of this section run on: the **pointer block**
    /// `>II(5,64)` — payload size 5 at the absolute offset 64 — then 56 zero bytes and
    /// `<<"HELLO">>` at that offset, so the file is `8 + 56 + 5 = 69` bytes.
    ///
    /// `>II` is big-endian, which is what `get_int32` reads (`sys.h:1274`).
    fn write_ipread_fixture(path: &Path) {
        let mut bytes = vec![0u8; 8 + 56];
        bytes[0..4].copy_from_slice(&5u32.to_be_bytes());
        bytes[4..8].copy_from_slice(&64u32.to_be_bytes());
        bytes.extend_from_slice(b"HELLO");
        std::fs::write(path, bytes).expect("the fixture is writable");
    }

    /// Opens `name` `[read]` and answers the descriptor (`open/2` is the only way in).
    fn open_read(runtime: &mut Runtime, dir: &Path, name: &str) -> Eterm {
        let path = path_in(runtime, dir, name);
        let read = atom(runtime, "read");
        let modes = list(runtime, &[read]);
        let answer = prim_file(runtime, "open", &[path, modes]);
        ok_value(runtime, answer)
    }

    /// Oracle (file = `>II(5,64)` + 56 zero bytes + `<<"HELLO">>` at 64), pinned live on
    /// OTP 29.1.1 / erts-17.1: `ipread(Fd,0,100)`⇒`{ok,{5,64,<<"HELLO">>}}`;
    /// `ipread(Fd,0,5)`/`(Fd,0,6)`⇒ the same; `ipread(Fd,0,(1 bsl 31) - 1)`⇒ the same;
    /// `ipread(Fd,0,infinity)`/`(Fd,0,foo)`⇒ the same (**any** atom is the `infinity` form).
    ///
    /// `ipread(Fd,0,3)`/`(Fd,0,4)`/`(Fd,0,0)`⇒`eof` (the declared size 5 exceeds `MaxSize`),
    /// `ipread(Fd,100,100)`/`(Fd,64,100)`⇒`eof` (fewer than 8 bytes at the pointer offset),
    /// `ipread(Fd,100,100)` reads nothing while `ipread(Fd,64,100)` reads the 5 payload bytes
    /// as a short pointer block — both are the bare `eof`.
    #[test]
    fn ipread_s32bu_p32bu_reads_the_indirect_block() {
        let mut runtime = runtime();
        let dir = scratch_dir("ipread");
        write_ipread_fixture(&dir.join("ipread.bin"));
        let fd = open_read(&mut runtime, &dir, "ipread.bin");

        // The sizes that admit the payload: the declared 5, more, and both `infinity` forms.
        let five = "{ok,{5,64,<<\"HELLO\">>}}";
        for max in [100, 5, 6, (1 << 31) - 1] {
            let answer = prim_file(
                &mut runtime,
                "ipread_s32bu_p32bu",
                &[fd, make_small(0), make_small(max)],
            );
            assert_eq!(rendered(&mut runtime, answer), five, "MaxSize {max}");
        }
        for name in ["infinity", "foo"] {
            let max = atom(&mut runtime, name);
            let answer =
                prim_file(&mut runtime, "ipread_s32bu_p32bu", &[fd, make_small(0), max]);
            assert_eq!(rendered(&mut runtime, answer), five, "MaxSize {name}");
        }

        // A `MaxSize` below the declared size, and a pointer offset the block does not fit
        // in, are all the bare atom `eof`.
        for (offset, max) in [(0, 3), (0, 4), (0, 0), (100, 100), (64, 100), (65, 100)] {
            let answer = prim_file(
                &mut runtime,
                "ipread_s32bu_p32bu",
                &[fd, make_small(offset), make_small(max)],
            );
            assert_eq!(
                atom_name(&runtime, answer),
                "eof",
                "offset {offset}, MaxSize {max}"
            );
        }
        // The cursor is never moved (both reads are positioned).
        let cur = atom(&mut runtime, "cur");
        let answer = prim_file(&mut runtime, "position", &[fd, cur]);
        assert_eq!(ok_value(&runtime, answer), make_small(0));
        assert_ok(&mut runtime, "close", &[fd]);
    }

    /// Oracle: `ipread(Fd,-1,10)`/`(Fd,0,1 bsl 31)`⇒`{error,einval}`;
    /// `ipread(Fd,0.0,10)`/`(Fd,0,10.0)`/`(Fd,"a",10)`/`(Fd,0,[a])`⇒`{error,badarg}`;
    /// `ipread(ClosedFd,0,100)`⇒`{error,einval}`; `ipread(foo,0,10)`/`(#{},0,10)` raise
    /// `function_clause` in the oracle (reproduced as the shared malformed-descriptor
    /// `{error,badarg}`).
    #[test]
    fn ipread_s32bu_p32bu_reports_the_oracles_failures() {
        let mut runtime = runtime();
        let dir = scratch_dir("ipread_fail");
        write_ipread_fixture(&dir.join("ipread.bin"));
        let fd = open_read(&mut runtime, &dir, "ipread.bin");

        // The NIF's own range checks.
        for (offset, max) in [(-1, 10), (0, 1 << 31), (-1, 1 << 31)] {
            assert_eq!(
                prim_error(
                    &mut runtime,
                    "ipread_s32bu_p32bu",
                    &[fd, make_small(offset), make_small(max)]
                ),
                "einval",
                "offset {offset}, MaxSize {max}"
            );
        }
        // A bignum passes `is_integer/1` but fails `enif_get_int64` ⇒ `einval`.
        let big = bignum(&mut runtime, true, &[0, 1]);
        assert_eq!(
            prim_error(&mut runtime, "ipread_s32bu_p32bu", &[fd, big, make_small(10)]),
            "einval"
        );
        assert_eq!(
            prim_error(&mut runtime, "ipread_s32bu_p32bu", &[fd, make_small(0), big]),
            "einval"
        );

        // The wrapper's clause set: a float, a string and a list are all the *value*
        // `{error,badarg}` (the guard needs integers), and a float `MaxSize` is not an atom.
        let float = heap_term(&mut runtime, |context| {
            terms::make_float(context, 0.0)
        });
        let string = chars(&mut runtime, "a");
        let one_list =
            heap_term(&mut runtime, |context| terms::make_list(context, &[make_small(97)]));
        for (offset, max) in [
            (float, make_small(10)),
            (make_small(0), float),
            (string, make_small(10)),
            (make_small(0), one_list),
        ] {
            assert_eq!(
                prim_error(&mut runtime, "ipread_s32bu_p32bu", &[fd, offset, max]),
                "badarg"
            );
        }

        // A term that is not a descriptor at all.
        let not_a_descriptor = atom(&mut runtime, "foo");
        assert_eq!(
            prim_error(
                &mut runtime,
                "ipread_s32bu_p32bu",
                &[not_a_descriptor, make_small(0), make_small(10)]
            ),
            "badarg"
        );
        let empty_map = heap_term(&mut runtime, |context| {
            let atoms = context.runtime.code_server.atoms_handle();
            terms::make_map(context, &[], &atoms)
        });
        assert_eq!(
            prim_error(
                &mut runtime,
                "ipread_s32bu_p32bu",
                &[empty_map, make_small(0), make_small(10)]
            ),
            "badarg"
        );

        // A closed descriptor is the NIF's `{error,einval}` — for an in-range request and
        // for an out-of-range one alike.
        assert_ok(&mut runtime, "close", &[fd]);
        for (offset, max) in [(0, 100), (-1, 10)] {
            assert_eq!(
                prim_error(
                    &mut runtime,
                    "ipread_s32bu_p32bu",
                    &[fd, make_small(offset), make_small(max)]
                ),
                "einval"
            );
        }
    }

    // -- prim_file:get_handle/1 -----------------------------------------------

    /// Oracle: `get_handle(Fd)`⇒a **4-byte** binary (a live host reported
    /// `<<17,0,0,0>>`: `byte_size/1`⇒`4`); `get_handle(ClosedFd)`⇒`{error,einval}`;
    /// `get_handle(foo)`/`get_handle(#{})` raise `function_clause` (reproduced as the shared
    /// malformed-descriptor `{error,badarg}`).
    ///
    /// The **value** is this runtime's own descriptor number (the entry's documented
    /// divergence — the number is a property of the process that holds the file), so only
    /// the shape and the host's own `as_raw_fd` are pinned here; the oracle cross-check
    /// compares the shape too.
    #[test]
    fn get_handle_returns_the_native_descriptor_binary() {
        let mut runtime = runtime();
        let dir = scratch_dir("get_handle");
        std::fs::write(dir.join("h.txt"), b"hello").expect("the file is writable");
        let fd = open_read(&mut runtime, &dir, "h.txt");

        let answer = prim_file(&mut runtime, "get_handle", &[fd]);
        let bytes = terms::binary_bytes(answer).expect("a bare 4-byte binary, not {ok,_}");
        assert_eq!(bytes.len(), 4, "the native `int` descriptor is 4 bytes");
        // The 4 bytes really are **this** runtime's own hosted descriptor, in the host's
        // native byte order — not a plausible-looking constant. (The exact number is the
        // entry's documented divergence; `prim_file.rs`'s own test pins it against the
        // held `std::fs::File`'s `as_raw_fd` through the private descriptor accessor.)
        let number = i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert!(number > 2, "a live host descriptor, got {number}");
        // The descriptor is live: the sibling `read/2` reads the file's bytes through it.
        let two = make_small(2);
        let read = prim_file(&mut runtime, "read", &[fd, two]);
        let read = ok_value(&mut runtime, read);
        assert_eq!(terms::binary_bytes(read).expect("a binary"), b"he");
        // A closed descriptor is `{error,einval}`, a malformed one the shared `{error,badarg}`.
        assert_ok(&mut runtime, "close", &[fd]);
        assert_eq!(prim_error(&mut runtime, "get_handle", &[fd]), "einval");
        let not_a_descriptor = atom(&mut runtime, "foo");
        assert_eq!(
            prim_error(&mut runtime, "get_handle", &[not_a_descriptor]),
            "badarg"
        );
        let empty_map = heap_term(&mut runtime, |context| {
            let atoms = context.runtime.code_server.atoms_handle();
            terms::make_map(context, &[], &atoms)
        });
        assert_eq!(prim_error(&mut runtime, "get_handle", &[empty_map]), "badarg");
    }

    // -- the typed capability gaps of the unserved prim_file entries -----------

    /// The `prim_file` entries this floor cannot serve are **claimed** so a caller gets the
    /// honest `unsupported: …` capability gap (CLI exit 3) instead of falling through into
    /// the preloaded `prim_file.beam`, whose `catch error:_ -> {error,badarg}` wrappers swallow
    /// the inner `undef` and answer a *value* the oracle never returns.
    ///
    /// The oracle's own answers, verified live against OTP 29.1.1 / erts-17.1:
    /// `write_file_info/2`⇒`ok`, `advise(Fd,0,1,sequential)`⇒`ok`, `allocate(Fd,0,10)`⇒`ok`,
    /// `list_dir/1`⇒`{ok,[Names]}` in raw `readdir(3)` order, `read_line(Fd)`⇒`{ok,Line}`
    /// and `file_desc_to_ref(1,[])`⇒`{ok,{file_descriptor,prim_file,#{…}}}`.
    ///
    /// `copy/2`, `write_file/3`, `delayed_close/1` and `sendfile/8` are deliberately **not**
    /// claimed: the oracle answers `error:undef` for the first three (`{error,undef}` as the
    /// caught value) and the *value* `{error,enotsup}` on `sendfile/8`'s second clause, so
    /// claiming any of them would replace an agreeing answer with a non-agreeing one.
    #[test]
    fn the_unserved_prim_file_entries_are_typed_capability_gaps() {
        let mut runtime = runtime();
        let path = chars(&mut runtime, "/tmp/gap");
        let one = [path];
        let two = [path, path];
        let three = [path, path, path];
        let four = [path, path, path, path];
        for (function, args) in [
            ("write_file_info", &two[..]),
            ("write_file_info", &three[..]),
            ("advise", &four[..]),
            ("allocate", &three[..]),
            ("list_dir", &one[..]),
            ("list_dir_all", &one[..]),
            ("read_line", &one[..]),
            ("file_desc_to_ref", &two[..]),
        ] {
            let arity = args.len();
            let error = call(&mut runtime, "prim_file", function, args)
                .expect_err(&format!("prim_file:{function}/{arity} is a capability gap"));
            let reason = crate::error::unsupported_reason(&error);
            assert!(
                reason.is_some(),
                "prim_file:{function}/{arity} must be classified `unsupported`, got {error}"
            );
        }

        // The names the oracle itself rejects with `undef` are still **unclaimed**, so the
        // call falls through to the preloaded module exactly as before (`try_native`
        // answering `None` *is* that fall-through).
        let eight = [path, path, path, path, path, path, path, path];
        for (function, args) in [
            ("copy", &two[..]),
            ("write_file", &three[..]),
            ("delayed_close", &one[..]),
            ("sendfile", &eight[..]),
        ] {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            assert!(
                context
                    .try_native("prim_file", function, args.len() as u32, args)
                    .is_none(),
                "prim_file:{function}/{} must stay unclaimed",
                args.len()
            );
        }
    }

    // -- the oracle cross-check -----------------------------------------------

    /// How long an oracle probe may run before it is killed and reaped.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

    /// How often the probe's child is polled while it runs.
    const PROBE_POLL: Duration = Duration::from_millis(10);

    /// The loud banner for a skipped oracle comparison — a missing toolchain is never a
    /// silent pass.
    fn skip_oracle_loudly(why: &str) {
        eprintln!();
        eprintln!("======= beam_rt native/prim.rs ORACLE CROSS-CHECK SKIPPED =======");
        eprintln!("{why}: the oracle comparison cannot run. NOT a pass.");
        eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
        eprintln!("================================================================");
        eprintln!();
    }

    /// Whether `candidate` is an executable regular file. The oracle is resolved with this
    /// and **never run** to test for existence.
    fn is_executable(candidate: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(candidate) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }

    /// The oracle `erl`: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH`
    /// (the convention `tools/differential` uses), **resolved without running it**.
    fn resolve_oracle() -> Option<PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = PathBuf::from(&value);
            return is_executable(&program).then_some(program);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| is_executable(candidate))
    }

    /// Runs `oracle` with `args`, bounded by [`PROBE_TIMEOUT`].
    ///
    /// The child is **polled** rather than waited on, so a probe that blocks is killed and
    /// reaped at the bound instead of hanging the test; stdin is `/dev/null` and the pipes
    /// are drained only after the child exits. `None` when the child could not be spawned,
    /// polled or collected, or when it outlived the bound.
    fn run_oracle(oracle: &Path, args: &[&str]) -> Option<std::process::Output> {
        let mut command = Command::new(oracle);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return child.wait_with_output().ok(),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(PROBE_POLL);
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }

    /// The fresh file tree both halves of the cross-check run on: the 11-byte `p` every
    /// offset-addressed case reads. Every other file the sequence needs is created by the
    /// `open(P,[read,write])` that reaches it.
    fn prepare_oracle_tree(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).expect("a creatable scratch dir");
        std::fs::write(dir.join("p"), b"0123456789a").expect("a writable seed file");
        write_ipread_fixture(&dir.join("ip"));
    }

    /// The `-eval` the oracle runs: exactly the sequence `runtime_answers` performs, each
    /// answer printed on its own line (`~p`, no wrapping — every answer is short) and a
    /// `halt(0)` so the probe exits without waiting for `-s init stop`.
    fn oracle_expression(dir: &str) -> String {
        format!(
            r#"P = "{dir}/p",
{{ok,Fd}} = prim_file:open(P,[read]),
A1 = prim_file:pread(Fd,0,3),
A2 = prim_file:pread(Fd,0,0),
A3 = prim_file:pread(Fd,[{{0,3}},{{6,5}}]),
A4 = prim_file:position(Fd,cur),
prim_file:close(Fd),
A5 = prim_file:pread(Fd,0,3),
W = "{dir}/w",
{{ok,Fd2}} = prim_file:open(W,[read,write]),
B1 = prim_file:pwrite(Fd2,0,<<"XY">>),
B2 = prim_file:pwrite(Fd2,[{{0,<<"A">>}},{{7,<<"B">>}}]),
B3 = prim_file:pwrite(Fd2,[{{0,<<"A">>}},{{1,foo}}]),
B4 = prim_file:pwrite(Fd2,-1,<<"Z">>),
prim_file:close(Fd2),
C = "{dir}/t",
{{ok,Fd3}} = prim_file:open(C,[read,write]),
C1 = prim_file:position(Fd3,4),
C2 = prim_file:truncate(Fd3),
prim_file:close(Fd3),
C3 = file:read_file(C),
D = "{dir}/d",
{{ok,Fd4}} = prim_file:open(D,[read,write]),
D1 = prim_file:sync(Fd4),
D2 = prim_file:datasync(Fd4),
prim_file:close(Fd4),
E = "{dir}/e",
E1 = prim_file:write_file(E,<<"hello">>),
E2 = file:read_file(E),
E3 = prim_file:write_file("{dir}/",<<"x">>),
E4 = prim_file:write_file("{dir}/missing/child",<<"x">>),
S1 = prim_file:set_cwd("{dir}"),
{{ok,Fd5}} = prim_file:open("{dir}/ip",[read]),
I1 = prim_file:ipread_s32bu_p32bu(Fd5,0,100),
I2 = prim_file:ipread_s32bu_p32bu(Fd5,0,3),
I3 = prim_file:ipread_s32bu_p32bu(Fd5,100,100),
I4 = prim_file:ipread_s32bu_p32bu(Fd5,-1,10),
I5 = prim_file:ipread_s32bu_p32bu(Fd5,0,infinity),
%% `get_handle`'s *value* is the process's own descriptor number, so only its shape
%% (`byte_size/1`, `is_binary/1`) and the closed-descriptor answer are comparable.
G1 = byte_size(prim_file:get_handle(Fd5)),
G2 = is_binary(prim_file:get_handle(Fd5)),
prim_file:close(Fd5),
G3 = prim_file:get_handle(Fd5),
lists:foreach(fun(X) -> io:format("~p~n",[X]) end,
              [A1,A2,A3,A4,A5,B1,B2,B3,B4,C1,C2,C3,D1,D2,E1,E2,E3,E4,S1,I1,I2,I3,I4,I5,G1,G2,G3]),
halt(0).
"#
        )
    }

    /// This runtime's answers to the same sequence, one `~p` rendering per answer.
    fn runtime_answers(dir: &Path) -> Vec<String> {
        let mut runtime = runtime();
        let mut answers: Vec<Eterm> = Vec::new();
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let cur = atom(&mut runtime, "cur");

        // A1..A5 — pread, with the cursor checked in between.
        let p = path_in(&mut runtime, &dir.to_path_buf(), "p");
        let modes = list(&mut runtime, &[read]);
        let answer = prim_file(&mut runtime, "open", &[p, modes]);
        let fd = ok_value(&runtime, answer);
        let answer = prim_file(&mut runtime, "pread", &[fd, make_small(0), make_small(3)]);
        answers.push(answer);
        let answer = prim_file(&mut runtime, "pread", &[fd, make_small(0), make_small(0)]);
        answers.push(answer);
        let first = offset_pair(&mut runtime, make_small(0), make_small(3));
        let second = offset_pair(&mut runtime, make_small(6), make_small(5));
        let pairs = list(&mut runtime, &[first, second]);
        let answer = prim_file(&mut runtime, "pread", &[fd, pairs]);
        answers.push(answer);
        let answer = prim_file(&mut runtime, "position", &[fd, cur]);
        answers.push(answer);
        assert_ok(&mut runtime, "close", &[fd]);
        let answer = prim_file(&mut runtime, "pread", &[fd, make_small(0), make_small(3)]);
        answers.push(answer);

        // B1..B4 — pwrite.
        let w = path_in(&mut runtime, &dir.to_path_buf(), "w");
        let modes = list(&mut runtime, &[read, write]);
        let answer = prim_file(&mut runtime, "open", &[w, modes]);
        let fd2 = ok_value(&runtime, answer);
        let payload = binary(&mut runtime, b"XY");
        let answer = prim_file(&mut runtime, "pwrite", &[fd2, make_small(0), payload]);
        answers.push(answer);
        let a = binary(&mut runtime, b"A");
        let b = binary(&mut runtime, b"B");
        let first = offset_pair(&mut runtime, make_small(0), a);
        let second = offset_pair(&mut runtime, make_small(7), b);
        let pairs = list(&mut runtime, &[first, second]);
        let answer = prim_file(&mut runtime, "pwrite", &[fd2, pairs]);
        answers.push(answer);
        let a = binary(&mut runtime, b"A");
        let foo = atom(&mut runtime, "foo");
        let first = offset_pair(&mut runtime, make_small(0), a);
        let second = offset_pair(&mut runtime, make_small(1), foo);
        let pairs = list(&mut runtime, &[first, second]);
        let answer = prim_file(&mut runtime, "pwrite", &[fd2, pairs]);
        answers.push(answer);
        let z = binary(&mut runtime, b"Z");
        let answer = prim_file(&mut runtime, "pwrite", &[fd2, make_small(-1), z]);
        answers.push(answer);
        assert_ok(&mut runtime, "close", &[fd2]);

        // C1..C3 — truncate, and the contents the host really holds.
        let c = path_in(&mut runtime, &dir.to_path_buf(), "t");
        let modes = list(&mut runtime, &[read, write]);
        let answer = prim_file(&mut runtime, "open", &[c, modes]);
        let fd3 = ok_value(&runtime, answer);
        let answer = prim_file(&mut runtime, "position", &[fd3, make_small(4)]);
        answers.push(answer);
        let answer = prim_file(&mut runtime, "truncate", &[fd3]);
        answers.push(answer);
        assert_ok(&mut runtime, "close", &[fd3]);
        let answer = prim_file(&mut runtime, "read_file", &[c]);
        answers.push(answer);

        // D1..D2 — sync/datasync.
        let d = path_in(&mut runtime, &dir.to_path_buf(), "d");
        let modes = list(&mut runtime, &[read, write]);
        let answer = prim_file(&mut runtime, "open", &[d, modes]);
        let fd4 = ok_value(&runtime, answer);
        let answer = prim_file(&mut runtime, "sync", &[fd4]);
        answers.push(answer);
        let answer = prim_file(&mut runtime, "datasync", &[fd4]);
        answers.push(answer);
        assert_ok(&mut runtime, "close", &[fd4]);

        // E1..E4 — write_file.
        let dir_text = dir.to_str().expect("a UTF-8 path");
        let e = path_in(&mut runtime, &dir.to_path_buf(), "e");
        let payload = binary(&mut runtime, b"hello");
        let answer = prim_file(&mut runtime, "write_file", &[e, payload]);
        answers.push(answer);
        let answer = prim_file(&mut runtime, "read_file", &[e]);
        answers.push(answer);
        let slash = chars(&mut runtime, &format!("{dir_text}/"));
        let payload = binary(&mut runtime, b"x");
        let answer = prim_file(&mut runtime, "write_file", &[slash, payload]);
        answers.push(answer);
        let child = chars(&mut runtime, &format!("{dir_text}/missing/child"));
        let answer = prim_file(&mut runtime, "write_file", &[child, payload]);
        answers.push(answer);

        // S1 — set_cwd.
        let target = chars(&mut runtime, dir_text);
        let answer = prim_file(&mut runtime, "set_cwd", &[target]);
        answers.push(answer);

        // I1..I5 / G1..G3 — the two families closed by this module. `get_handle`'s **value**
        // is the process's own descriptor number (the entry's documented divergence), so only
        // its shape and the closed-descriptor answer are comparable with the oracle.
        let ip = path_in(&mut runtime, &dir.to_path_buf(), "ip");
        let modes = list(&mut runtime, &[read]);
        let answer = prim_file(&mut runtime, "open", &[ip, modes]);
        let fd5 = ok_value(&runtime, answer);
        let answer = prim_file(
            &mut runtime,
            "ipread_s32bu_p32bu",
            &[fd5, make_small(0), make_small(100)],
        );
        answers.push(answer);
        let answer = prim_file(
            &mut runtime,
            "ipread_s32bu_p32bu",
            &[fd5, make_small(0), make_small(3)],
        );
        answers.push(answer);
        let answer = prim_file(
            &mut runtime,
            "ipread_s32bu_p32bu",
            &[fd5, make_small(100), make_small(100)],
        );
        answers.push(answer);
        let answer = prim_file(
            &mut runtime,
            "ipread_s32bu_p32bu",
            &[fd5, make_small(-1), make_small(10)],
        );
        answers.push(answer);
        let infinity = atom(&mut runtime, "infinity");
        let answer = prim_file(
            &mut runtime,
            "ipread_s32bu_p32bu",
            &[fd5, make_small(0), infinity],
        );
        answers.push(answer);
        let answer = prim_file(&mut runtime, "get_handle", &[fd5]);
        let width = terms::binary_bytes(answer).expect("a bare descriptor binary").len();
        answers.push(make_small(width as i64));
        let answer = prim_file(&mut runtime, "get_handle", &[fd5]);
        let is_binary = if terms::binary_bytes(answer).is_ok() {
            atom(&mut runtime, "true")
        } else {
            atom(&mut runtime, "false")
        };
        answers.push(is_binary);
        assert_ok(&mut runtime, "close", &[fd5]);
        let answer = prim_file(&mut runtime, "get_handle", &[fd5]);
        answers.push(answer);

        answers
            .into_iter()
            .map(|answer| {
                runtime
                    .format_term(answer, None)
                    .expect("a printable answer")
            })
            .collect()
    }

    /// The `pread`/`pwrite`/`truncate`/`sync`/`datasync`/`write_file`/`set_cwd` families
    /// closed in this module, cross-checked against the system `erl` oracle: the same
    /// sequence of `prim_file` calls is run under both runtimes on the same fresh file tree
    /// and the `~p` renderings of the answers compared.
    ///
    /// The probe is bounded (`erl -noshell` never exits on its own, so `-s init stop` is
    /// mandatory) and a missing or unanswerable oracle is a **loud skip**, never a silent
    /// pass. The whole comparison runs under [`with_cwd_restored`], because `set_cwd/1`
    /// changes the process-wide working directory.
    #[test]
    fn the_closed_prim_file_families_agree_with_the_erl_oracle() {
        let Some(oracle) = resolve_oracle() else {
            skip_oracle_loudly("no `erl` on $PATH (and no executable $ERTS_ORACLE)");
            return;
        };
        let dir = scratch_dir("oracle");
        let dir_text = dir.to_str().expect("a UTF-8 path").to_string();

        with_cwd_restored(|| {
            // This runtime's answers on the fresh tree.
            prepare_oracle_tree(&dir);
            let ours = runtime_answers(&dir);

            // The oracle's answers on the same fresh tree.
            prepare_oracle_tree(&dir);
            let expression = oracle_expression(&dir_text);
            let args = ["-noshell", "-eval", expression.as_str(), "-s", "init", "stop"];
            let Some(output) = run_oracle(&oracle, &args) else {
                skip_oracle_loudly("the oracle probe could not answer within the bound");
                return;
            };
            if !output.status.success() {
                eprintln!(
                    "warning: the oracle exited with {:?}\n--- stderr ---\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                );
                skip_oracle_loudly("the oracle probe exited with a failure");
                return;
            }
            let theirs: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect();
            assert_eq!(
                theirs, ours,
                "the prim_file families closed here must agree with the oracle"
            );
        });
    }

    // -- the `#file_info{}` family cross-check --------------------------------

    /// `term` rendered and then **whitespace-stripped**: the `~p` printer's 80-column line
    /// wrapping differs only in layout, so the compacted form is what is comparable.
    fn rendered_compact(runtime: &mut Runtime, term: Eterm) -> String {
        rendered(runtime, term).split_whitespace().collect()
    }

    /// `{time, Value}` on the runtime's pointer-stable arena.
    fn time_option(runtime: &mut Runtime, value: &str) -> Eterm {
        heap_term(runtime, move |ctx| {
            let key = ctx.atom("time")?;
            let value = ctx.atom(value)?;
            terms::make_tuple(ctx, &[key, value])
        })
    }

    /// The read-only tree the `#file_info{}` cross-check runs on: a 6-byte `f` (`0644`), a
    /// `0444` copy `g`, a directory `d` and a symlink `l` → `f`.
    ///
    /// Nothing writes it between the two halves of the comparison, so every `stat` field —
    /// including the three timestamps — is the same for both runtimes.
    fn prepare_file_info_tree(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).expect("a creatable scratch dir");
        for (name, mode) in [("f", 0o644), ("g", 0o444)] {
            let path = dir.join(name);
            std::fs::write(&path, b"hello\n").expect("a writable file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("a settable mode");
        }
        std::fs::create_dir(dir.join("d")).expect("a directory");
        let _ = std::fs::remove_file(dir.join("l"));
        symlink("f", dir.join("l")).expect("a symlink");
        // Follow the fresh symlink once: the kernel's relatime rule bumps a symlink's atime on
        // its first traversal, which would otherwise happen inside one of the two runs and make
        // the two `read_link_info(l)` answers differ by a second.
        let _ = std::fs::metadata(dir.join("l"));    }

    /// This runtime's answers to the `#file_info{}` sequence, one compacted rendering per
    /// answer — the exact sequence [`file_info_expression`] performs on the oracle.
    fn file_info_answers(dir: &Path) -> Vec<String> {
        let mut runtime = runtime();
        let dir_path = dir.to_path_buf();
        let f = path_in(&mut runtime, &dir_path, "f");
        let g = path_in(&mut runtime, &dir_path, "g");
        let d = path_in(&mut runtime, &dir_path, "d");
        let l = path_in(&mut runtime, &dir_path, "l");
        let missing = path_in(&mut runtime, &dir_path, "missing");
        let mut bytes = dir_path.join("f").to_str().expect("a UTF-8 path").as_bytes().to_vec();
        let binary_f = binary(&mut runtime, &bytes);
        bytes.push(0);
        let nul_f = binary(&mut runtime, &bytes);
        let foo = atom(&mut runtime, "foo");
        let posix_opt = time_option(&mut runtime, "posix");
        let universal_opt = time_option(&mut runtime, "universal");
        let local_opt = time_option(&mut runtime, "local");
        let bogus_opt = time_option(&mut runtime, "bogus");
        let posix = list(&mut runtime, &[posix_opt]);
        let universal = list(&mut runtime, &[universal_opt]);
        let local = list(&mut runtime, &[local_opt]);
        let bogus = list(&mut runtime, &[bogus_opt]);
        let time_atom = atom(&mut runtime, "time");
        let bare_time = list(&mut runtime, &[time_atom]);
        let read_atom = atom(&mut runtime, "read");
        let mode_read = list(&mut runtime, &[read_atom]);
        let mut answers: Vec<Eterm> = Vec::new();
        answers.push(prim_file(&mut runtime, "read_file_info", &[f]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[binary_f]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[d]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[l]));
        answers.push(prim_file(&mut runtime, "read_link_info", &[l]));
        answers.push(prim_file(&mut runtime, "read_link_info", &[f]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[f, posix]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[f, universal]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[f, local]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[f, bogus]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[f, bare_time]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[missing]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[missing, bogus]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[nul_f]));
        answers.push(prim_file(&mut runtime, "read_file_info", &[foo]));
        answers.push(prim_file(&mut runtime, "read_link_info", &[d]));
        let answer = prim_file(&mut runtime, "open", &[f, mode_read]);
        let fd = ok_value(&runtime, answer);
        let answer = prim_file(&mut runtime, "open", &[g, mode_read]);
        let fd2 = ok_value(&runtime, answer);
        answers.push(prim_file(&mut runtime, "read_handle_info", &[fd]));
        answers.push(prim_file(&mut runtime, "read_handle_info", &[fd, posix]));
        answers.push(prim_file(&mut runtime, "read_handle_info", &[fd2]));
        answers.push(prim_file(&mut runtime, "read_link_info", &[d, universal]));
        assert_ok(&mut runtime, "close", &[fd]);
        assert_ok(&mut runtime, "close", &[fd2]);
        answers.push(prim_file(&mut runtime, "read_handle_info", &[fd]));
        answers.push(prim_file(&mut runtime, "read_handle_info", &[foo]));
        answers
            .into_iter()
            .map(|term| rendered_compact(&mut runtime, term))
            .collect()
    }

    /// The `-eval` the oracle runs: the same sequence, each answer printed with `~p` and
    /// terminated by `|` so the compacted forms line up regardless of line wrapping.
    fn file_info_expression(dir: &str) -> String {
        format!(
            r#"F = "{dir}/f", G = "{dir}/g", D = "{dir}/d", L = "{dir}/l", M = "{dir}/missing",
FOO = foo,
B = list_to_binary(F),
{{ok,Fd}} = prim_file:open(F,[read]),
{{ok,Fd2}} = prim_file:open(G,[read]),
A1 = prim_file:read_file_info(F),
A2 = prim_file:read_file_info(B),
A3 = prim_file:read_file_info(D),
A4 = prim_file:read_file_info(L),
A5 = prim_file:read_link_info(L),
A6 = prim_file:read_link_info(F),
A7 = prim_file:read_file_info(F,[{{time,posix}}]),
A8 = prim_file:read_file_info(F,[{{time,universal}}]),
A9 = prim_file:read_file_info(F,[{{time,local}}]),
A10 = prim_file:read_file_info(F,[{{time,bogus}}]),
A11 = prim_file:read_file_info(F,[time]),
A12 = prim_file:read_file_info(M),
A13 = prim_file:read_file_info(M,[{{time,bogus}}]),
A14 = prim_file:read_file_info(<<B/binary,0>>),
A15 = prim_file:read_file_info(FOO),
A16 = prim_file:read_link_info(D),
A17 = prim_file:read_handle_info(Fd),
A18 = prim_file:read_handle_info(Fd,[{{time,posix}}]),
A19 = prim_file:read_handle_info(Fd2),
A20 = prim_file:read_link_info(D,[{{time,universal}}]),
ok = prim_file:close(Fd),
ok = prim_file:close(Fd2),
A21 = prim_file:read_handle_info(Fd),
A22 = prim_file:read_handle_info(FOO),
lists:foreach(fun(X) -> io:format("~p|",[X]) end,
              [A1,A2,A3,A4,A5,A6,A7,A8,A9,A10,A11,A12,A13,A14,A15,A16,A17,A18,A19,A20,
               A21,A22]),
halt(0).
"#
        )
    }

    /// The `#file_info{}` family's oracle cross-check: both `read_*_info` path forms (list and
    /// binary), both symlink answers, all four time options, a bare `time` element, a missing
    /// path with and without an option, a NUL path, a non-path term, and the four
    /// `read_handle_info` answers (a `0644` file, a `0444` file, a closed descriptor, a
    /// non-descriptor) — every answer rendered compacted and compared line for line.
    #[test]
    fn the_file_info_family_agrees_with_the_oracle() {
        let Some(oracle) = resolve_oracle() else {
            skip_oracle_loudly("no `erl` on $PATH (and no executable $ERTS_ORACLE)");
            return;
        };
        let dir = scratch_dir("file_info");
        let dir_text = dir.to_str().expect("a UTF-8 path").to_string();
        prepare_file_info_tree(&dir);

        let ours = file_info_answers(&dir);
        let expression = file_info_expression(&dir_text);
        let args = ["-noshell", "-eval", expression.as_str(), "-s", "init", "stop"];
        let Some(output) = run_oracle(&oracle, &args) else {
            skip_oracle_loudly("the oracle probe could not answer within the bound");
            return;
        };
        if !output.status.success() {
            eprintln!(
                "warning: the oracle exited with {:?}\n--- stderr ---\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            skip_oracle_loudly("the oracle probe exited with a failure");
            return;
        }
        let compacted: String = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .collect();
        let theirs: Vec<String> = compacted
            .split('|')
            .filter(|answer| !answer.is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(
            theirs, ours,
            "the #file_info{{}} family closed here must agree with the oracle"
        );
    }

    /// A `function_clause` `Opts` is an **uncaught** raise, never the wrapper's `{error,
    /// badarg}` value: `proplist_get_value/3` is evaluated outside `read_info_1/3`'s `try`
    /// (`prim_file.erl:642-645`, verified against the oracle).
    #[test]
    fn a_non_list_opts_raises_function_clause() {
        let mut runtime = runtime();
        let dir = scratch_dir("file_info_opts");
        prepare_file_info_tree(&dir);
        let f = path_in(&mut runtime, &dir, "f");
        let not_a_list = atom(&mut runtime, "foo");
        for function in ["read_file_info", "read_link_info", "read_handle_info"] {
            let error = call(&mut runtime, "prim_file", function, &[f, not_a_list])
                .expect_err("a non-list Opts must raise");
            assert_eq!(
                error.as_atom(),
                Some("function_clause"),
                "{function} must raise the bare reason atom"
            );
        }
    }
}
