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
//! The rest of `prim_file` (`open`/`read`/`write`/`position`/`stat`/…) still falls through
//! to the preloaded `.beam`. Deliberately unclaimed here, with the reason:
//! `truncate/1`, `sync/1`, `datasync/1`, `advise/4` (they take a `#file_descriptor{}` from
//! `open`, which needs the open-file resource); `write_file/2` (pure Erlang over
//! `open`/`write`/`close`); `copy/2` (does not exist — only `copy/3`); `list_dir/1` (its
//! entries come back in `readdir(3)` order, which the oracle does not sort and this runtime
//! cannot pin); and the whole `read_file_info/1,2` + `read_link_info/1,2` family (needs
//! `access(2)`, which safe `std` does not expose, plus the local-time conversion the
//! runtime lacks). `prim_zip:splitter/3` and the other `prim_*` modules (`prim_buffer`,
//! `prim_eval`, `prim_inet`/`prim_socket`/`prim_net`, `erl_prim_loader`) are untouched —
//! see `CONTEXT.md` for the machinery each needs.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt};

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, is_nil, is_small, signed_value, Eterm};

use super::RuntimeContext;

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
    fn encode_path(&self, term: Eterm) -> Option<Vec<u8>> {
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
    fn ok_atom(&mut self) -> Result<Eterm> {
        self.atom("ok")
    }

    /// `{ok, Value}`.
    fn ok_tuple(&mut self, value: Eterm) -> Result<Eterm> {
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, value])
    }

    /// `{error, Reason}` for the literal atom `reason`.
    fn error_tuple(&mut self, reason: &str) -> Result<Eterm> {
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
    fn error_tuple_for(&mut self, error: &io::Error) -> Result<Eterm> {
        self.error_tuple_errno(error.raw_os_error().unwrap_or(0))
    }
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
fn errno_name(code: i32) -> String {
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
