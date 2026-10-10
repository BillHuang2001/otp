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
//! # The reply the oracle builds
//!
//! `file_server.erl:141-145`/`:156-160` answers these requests with
//! `?PRIM_FILE:read_file_info(Name)`/`read_link_info(Name)`, i.e. `prim_file`'s
//! `read_info_1(Name, FollowLinks, TimeType)` (`erts/preloaded/src/prim_file.erl:647-665`) with
//! `FollowLinks` = `1`/`0`. That calls `read_info_nif` and reshapes its result through
//! `adjust_times/2`; the NIF emits the 14-tuple
//! `{file_info, size, type, access, atime, mtime, ctime, mode, links, major_device,
//! minor_device, inode, uid, gid}` (`erts/emulator/nifs/common/prim_file_nif.c:996-1017`,
//! `build_file_info/2`).
//!
//! # Why only the failure path is served
//!
//! Thirteen of the fourteen fields come straight out of the `stat(2)`/`lstat(2)` result this
//! module already performs. The **`access`** slot does not, and it is privilege-sensitive:
//!
//! * the path forms (what this module serves) resolve it with two real **`access(2)`** calls,
//!   `R_OK` then `W_OK` — `erts/emulator/nifs/unix/unix_prim_file.c:739-769`, `efile_read_info`.
//!   `access(2)` consults the calling process's real uid/gid, the mount's read-only flag and any
//!   ACL, so it is **not** a function of `st_mode` alone, and it is reachable only through
//!   `libc::access` — which `beam_rt`'s `#![deny(unsafe_code)]` forbids.
//! * the handle form (`efile_read_handle_info` → `check_access`, `:771-801`) derives it from
//!   `geteuid()`/`getegid()` against the mode bits, and no safe `std` API exposes the effective
//!   uid/gid either.
//!
//! Deriving the bits from `MetadataExt::mode()` plus the file's *own* `uid` would be a
//! **fabrication**, not an approximation, because the oracle's answer changes with privilege:
//! on a host where this runtime is root (a `unshare -r` / `nix develop` user namespace), a
//! mode-`000` file owned by that root answers `read_write` — root bypasses the mode bits in
//! `access(2)` — while any owner-uid derivation answers `none`. Handing that wrong record to
//! `filelib:is_dir/1` turns a truthful `true`/`false` into a **divergence**, so the success path
//! stays the typed capability gap [`serve`] reports.
//!
//! The failure path has no such problem: `stat`/`lstat` fail *before* a `#file_info{}` exists,
//! and `read_info_1/3` returns that `{error, Reason}` (`prim_file.erl:660`) before it ever looks
//! at the time option — so the oracle's `{error, enoent}` (its
//! `posix_error_to_tuple(erl_errno_id(errno))`) is exactly reproducible from
//! `std::io::Error::raw_os_error()`.
//!
//! Closing the success path needs one capability from **`beam_io`** — the crate that is allowed
//! `unsafe`: a safe `path_access(Path) -> (read, write)` wrapping `access(2)` (which also closes
//! the whole `#file_info{}` family in `crate::native::prim`), plus a
//! `process_credentials() -> (euid, egid)` for `read_handle_info/1,2`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use beam_common::Result;
use beam_term::prelude::*;

use crate::error;
use crate::runtime::Runtime;

/// Serves `{read_file_info, Name}` (`follow_links`) or `{read_link_info, Name}`
/// (`!follow_links`) — `file_server.erl:141-145` / `:156-160`, i.e. `prim_file`'s
/// `read_info_1(Name, FollowLinks, TimeType)` with `1` / `0` (`prim_file.erl:647-655`).
///
/// A `stat`/`lstat` failure is the oracle's own `{error, Reason}` — the same syscall, so the
/// same errno; a path `prim_file:encode_path/1` rejects is `{error, badarg}`, exactly as the
/// sibling `{open, …}`/`{list_dir, …}` arms answer it. A **successful** stat is the typed
/// capability gap the module docs justify, never a `#file_info{}` with an invented `access`.
///
/// # Errors
///
/// [`crate::error::unsupported`] for every path that stats successfully (the `access` slot is
/// not derivable from safe Rust), and the runtime's allocation diagnostics.
pub(super) fn serve(runtime: &mut Runtime, follow_links: bool, args: &[Eterm]) -> Result<Eterm> {
    let Some(path) = runtime.encode_path(args[0]) else {
        return runtime.error_tuple("badarg");
    };
    let os_path = OsStr::from_bytes(&path);
    // `FollowLinks` is the oracle's `stat`/`lstat` choice (`unix_prim_file.c:742-750`).
    let stat = if follow_links {
        std::fs::metadata(os_path)
    } else {
        std::fs::symlink_metadata(os_path)
    };
    match stat {
        Err(failure) => runtime.error_tuple_for(&failure),
        Ok(_metadata) => Err(error::unsupported(
            "file_server: the #file_info{} `access` slot needs access(2), which no safe std API \
             exposes",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// The success path is the precise typed capability gap: it never produces a
    /// `#file_info{}`, so `access` can never be invented. The reason names the missing
    /// capability (`access(2)`), which is what a future `beam_io` facility would close.
    #[test]
    fn a_successful_stat_is_the_access_capability_gap() {
        let mut runtime = runtime();
        let file = TempFile::new("existing");
        for follow_links in [true, false] {
            let term = path_term(&mut runtime, file.path());
            let error = serve(&mut runtime, follow_links, &[term])
                .expect_err("a successful stat must stay a typed gap");
            assert_eq!(
                crate::error::unsupported_reason(&error).as_deref(),
                Some(
                    "file_server: the #file_info{} `access` slot needs access(2), which no safe \
                     std API exposes"
                ),
                "the gap names the missing `access(2)` capability"
            );
        }
    }
}
