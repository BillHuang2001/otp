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

//! The `#file_info{}` readers: `{read_file_info, Name}` and `{read_link_info, Name}`.
//!
//! These are the two requests `file:read_file_info/1` (`lib/kernel/src/file.erl:665-666`) and
//! `file:read_link_info/1` (`:821-822`) send to the `file_server_2` service through
//! `gen_server:call/3`, and therefore the seam every `filelib` existence probe goes through:
//! `filelib:is_dir/1`, `filelib:is_file/1` and `filelib:is_regular/1` read only
//! `#file_info.type` (`lib/stdlib/src/filelib.erl:273-297`, via `eval_read_file_info/2`,
//! `:772-773`), and `filelib:wildcard/1`'s fully-literal case goes through `exists/2` →
//! `file:read_link_info/1` (`:742-753` + `eval_read_link_info/2`, `:782-783`).
//!
//! # The reply
//!
//! `file_server.erl:141-145`/`:156-160` answers these requests with
//! `?PRIM_FILE:read_file_info(Name [, Opts])`/`read_link_info(Name [, Opts])`, i.e.
//! `prim_file`'s `read_info_1(Name, FollowLinks, TimeType)` (`erts/preloaded/src/prim_file.erl:647-663`)
//! with `FollowLinks` = `1`/`0`. That calls `read_info_nif` and reshapes its result through
//! `adjust_times/2`; the NIF emits the 14-tuple
//! `{file_info, size, type, access, atime, mtime, ctime, mode, links, major_device,
//! minor_device, inode, uid, gid}` (`erts/emulator/nifs/common/prim_file_nif.c:996-1017`,
//! `build_file_info/2`).
//!
//! Both halves are served here, through the **one** builder the sibling
//! [`crate::native::prim`] module owns ([`prim::file_info_term`]), so the `#file_info{}`
//! layout, the `access` derivation and the time conversions cannot drift:
//!
//! * the **failure** path is the `stat(2)`/`lstat(2)` errno (`std::io::Error::raw_os_error`),
//!   exactly the input `posix_error_to_tuple(erl_errno_id(errno))` takes;
//! * the **success** path resolves `access` with two real `access(2)` probes (`R_OK` then
//!   `W_OK`, `erts/emulator/nifs/unix/unix_prim_file.c:739-769`) through
//!   [`beam_io::fs_access`], the crate that owns the `unsafe` FFI. `access(2)` consults the
//!   calling process's real uid/gid, the mount's read-only flag and any ACL, so it is **not**
//!   a function of `st_mode` alone — deriving the bits from the mode would be a fabrication
//!   (on a host where this runtime is root a mode-`000` file answers `read_write`).
//!
//! A third form, `{read_file_info, Name, Opts}`, carries the same `[{time, posix |
//! universal | local}]` option its `prim_file` counterpart takes; an unrecognised time type
//! is the wrapper's **value** `{error, badarg}`, and an `Opts` that is not a proper list is
//! that helper's uncaught `function_clause` raise.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use beam_common::Result;
use beam_term::prelude::*;

use crate::context::RuntimeContext;
use crate::native::prim;
use crate::runtime::Runtime;

/// Serves `{read_file_info, Name [, Opts]}` (`follow_links`) or
/// `{read_link_info, Name [, Opts]}` (`!follow_links`) — `file_server.erl:141-145` /
/// `:156-160`, i.e. `prim_file`'s `read_info_1(Name, FollowLinks, TimeType)` with `1` / `0`
/// (`prim_file.erl:647-655`).
///
/// The option is resolved first, because the oracle's `proplist_get_value(time, Opts,
/// local)` is evaluated **before** the path is encoded and outside the wrapper's `try`: a
/// non-proper-list `Opts` is an uncaught `function_clause` raise. Then a `stat`/`lstat`
/// failure is the oracle's own `{error, Reason}` (the same syscall, so the same errno), a
/// path `prim_file:encode_path/1` rejects is `{error, badarg}` — exactly as the sibling
/// `{open, …}`/`{list_dir, …}` arms answer it — and a successful stat is the real
/// `{ok, #file_info{}}`, `access` included.
///
/// # Errors
///
/// The `function_clause` raise described above, and the runtime's allocation diagnostics.
pub(super) fn serve(runtime: &mut Runtime, follow_links: bool, args: &[Eterm]) -> Result<Eterm> {
    let mut ctx = RuntimeContext::new(runtime);
    let Some(time) = ctx.file_info_time(args.get(1).copied()) else {
        return Err(prim::function_clause());
    };
    let Some(path) = ctx.encode_path(args[0]) else {
        return ctx.error_tuple("badarg");
    };
    let os_path = OsStr::from_bytes(&path);
    // `FollowLinks` is the oracle's `stat`/`lstat` choice (`unix_prim_file.c:742-750`).
    let stat = if follow_links {
        std::fs::metadata(os_path)
    } else {
        std::fs::symlink_metadata(os_path)
    };
    let metadata = match stat {
        Err(failure) => return ctx.error_tuple_for(&failure),
        Ok(metadata) => metadata,
    };
    let access = ctx.path_access_bits(os_path);
    ctx.file_info_reply(&metadata, access, time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_engine::terms;
    use crate::native::file_server::tests::{build, runtime};
    use crate::native::file_server::tuple_fields;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique path in the OS temp directory, from a process-wide counter so two tests in one
    /// binary can never collide.
    fn unique_path(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "beam_rt_file_info_{label}_{}_{unique}",
            std::process::id()
        ))
    }

    /// A real file in the OS temp directory, removed when the guard is dropped.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(label: &str) -> TempFile {
            let path = unique_path(label);
            std::fs::write(&path, b"x").expect("a temporary file");
            TempFile(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A char-list path term for `path`.
    fn path_term(runtime: &mut Runtime, path: &Path) -> Eterm {
        let text = path.to_str().expect("a UTF-8 temporary path");
        build(runtime, |ctx| ctx.charlist(text).expect("a char list"))
    }

    /// The `[name, name]` rendering of an `{error, Reason}` reply.
    fn error_fields(runtime: &mut Runtime, reply: Eterm) -> Vec<String> {
        tuple_fields(reply)
            .expect("a tuple")
            .iter()
            .map(|field| runtime.atom_name(*field).expect("an atom"))
            .collect()
    }

    /// A path that does not exist is the oracle's `{error, enoent}` for **both** requests:
    /// `stat(2)` and `lstat(2)` fail identically on an absent name.
    #[test]
    fn an_absent_path_is_the_oracles_error_tuple() {
        let mut runtime = runtime();
        let gone = unique_path("absent");
        assert_eq!(std::fs::metadata(&gone).is_err(), true, "the path is absent");
        for follow_links in [true, false] {
            let term = path_term(&mut runtime, &gone);
            let reply = serve(&mut runtime, follow_links, &[term]).expect("a reply");
            assert_eq!(error_fields(&mut runtime, reply), ["error", "enoent"]);
        }
    }

    /// A path whose parent is a **file** is the oracle's `{error, enotdir}`: the errno the
    /// service's own `stat(2)` produced is carried through unchanged (`errno_atom/1`).
    #[test]
    fn an_unstatable_path_carries_the_stat_errno() {
        let mut runtime = runtime();
        let parent = TempFile::new("enotdir");
        let child = parent.path().join("child");
        assert_eq!(
            std::fs::metadata(&child).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR),
            "the fixture must be a file used as a directory"
        );
        let term = path_term(&mut runtime, &child);
        let reply = serve(&mut runtime, true, &[term]).expect("a reply");
        assert_eq!(error_fields(&mut runtime, reply), ["error", "enotdir"]);
    }

    /// A path `prim_file` rejects is `{error, badarg}` — never a stat of a fabricated name.
    #[test]
    fn a_rejected_path_is_badarg() {
        let mut runtime = runtime();
        let atom = build(&mut runtime, |ctx| ctx.atom("nope").expect("an atom"));
        let reply = serve(&mut runtime, true, &[atom]).expect("a reply");
        assert_eq!(error_fields(&mut runtime, reply), ["error", "badarg"]);
    }

    /// The integer value of a `#file_info{}` field term.
    fn number(term: Eterm) -> i64 {
        assert!(is_small(term), "an integer field");
        beam_term::signed_value(term)
    }
    /// The success path is the real `{ok, #file_info{}}`: 14 fields whose metadata-derived
    /// values are read back from the same `stat(2)`/`lstat(2)` (`std::fs::metadata`), with the
    /// seven `enif_make_uint` fields **truncated to 32 bits** exactly as `build_file_info/2`
    /// emits them (`?PRIM_FILE_NIF:996-1017`). The whole-record live-oracle cross-check lives in
    /// `crate::native::prim::tests`; this pins the `file_server_2` request path onto the same
    /// record.
    #[test]
    fn an_existing_path_answers_the_real_file_info() {
        use std::os::unix::fs::MetadataExt;
        let mut runtime = runtime();
        let file = TempFile::new("existing");
        let metadata = std::fs::metadata(file.path()).expect("a stat");
        for follow_links in [true, false] {
            let term = path_term(&mut runtime, file.path());
            let reply = serve(&mut runtime, follow_links, &[term]).expect("a reply");
            let fields = tuple_fields(reply).expect("an {ok, _} tuple");
            assert_eq!(fields.len(), 2, "the ok wrapper");
            assert_eq!(runtime.atom_name(fields[0]).expect("an atom"), "ok");
            let fields = tuple_fields(fields[1]).expect("a #file_info{}");
            assert_eq!(fields.len(), 14, "the #file_info{{}} arity");
            assert_eq!(runtime.atom_name(fields[0]).expect("an atom"), "file_info");            assert_eq!(number(fields[1]), metadata.len() as i64);
            assert_eq!(runtime.atom_name(fields[2]).expect("an atom"), "regular");
            assert_eq!(runtime.atom_name(fields[3]).expect("an atom"), "read_write");
            for when in &fields[4..7] {
                assert_eq!(
                    tuple_fields(*when).expect("a datetime").len(),
                    2,
                    "each time field is a {{Y,M,D}}/{{H,M,S}} pair"
                );
            }
            assert_eq!(number(fields[7]), i64::from(metadata.mode()));
            assert_eq!(number(fields[8]), metadata.nlink() as i64);
            assert_eq!(number(fields[9]), i64::from(metadata.dev() as u32));
            assert_eq!(number(fields[10]), i64::from(metadata.rdev() as u32));
            assert_eq!(number(fields[11]), i64::from(metadata.ino() as u32));
            assert_eq!(number(fields[12]), i64::from(metadata.uid()));
            assert_eq!(number(fields[13]), i64::from(metadata.gid()));
        }
    }

    /// The `[{time, posix}]` option the wrapper's `adjust_times/2` recognises is carried
    /// through, and an unrecognised one is the wrapper's **value** `{error, badarg}` — after the
    /// stat, so the record is never built.
    #[test]
    fn the_time_option_selects_the_representation() {
        let mut runtime = runtime();
        let file = TempFile::new("time");
        let path = path_term(&mut runtime, file.path());
        let posix = time_option(&mut runtime, "posix");
        let reply = serve(&mut runtime, true, &[path, posix]).expect("a reply");
        let fields = tuple_fields(reply).expect("a tuple");
        assert_eq!(runtime.atom_name(fields[0]).expect("an atom"), "ok");
        let info = tuple_fields(fields[1]).expect("a #file_info{}");
        for when in &info[4..7] {
            assert_eq!(number(*when), metadata_seconds(&file), "raw POSIX seconds");
        }
        let bogus = time_option(&mut runtime, "bogus");
        let reply = serve(&mut runtime, true, &[path, bogus]).expect("a reply");
        assert_eq!(error_fields(&mut runtime, reply), ["error", "badarg"]);
    }

    /// `{time, Value}` on the runtime's arena.
    fn time_option(runtime: &mut Runtime, value: &str) -> Eterm {
        build(runtime, |ctx| {
            let key = ctx.atom("time").expect("an atom");
            let value = ctx.atom(value).expect("an atom");
            let pair = terms::make_tuple(ctx, &[key, value]).expect("a tuple");
            terms::make_list(ctx, &[pair]).expect("a list")
        })
    }
    /// The file's own `st_mtime` in whole POSIX seconds — the value a `[{time,posix}]` record
    /// reports (the NIF's `enif_int64` of `st_mtime`).
    fn metadata_seconds(file: &TempFile) -> i64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(file.path())
            .expect("a stat")
            .mtime()
    }
}
