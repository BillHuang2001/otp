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

//! Filesystem-access and process-credential facility — a safe `access(2)`/`geteuid(2)`/
//! `getegid(2)` floor.
//!
//! Two raw-OS questions the `file`/`filelib` floor has to answer are out of reach for safe
//! `std`, so they live here behind safe wrappers — the same role [`crate::signal`] plays for
//! `os:set_signal/2`, [`crate::cpu_time`] for `erlang:statistics(runtime)` and [`crate::ddll`]
//! for `erl_ddll` (the runtime crate `beam_rt` denies `unsafe_code` and cannot issue the
//! syscalls itself):
//!
//! * **May this process read/write this path?** — [`path_access`], the `#file_info{}` `access`
//!   slot; [`access`] is the single-check boolean form and [`try_access`] the errno-preserving
//!   one.
//! * **Who is this process?** — [`process_credentials`], the effective user/group id a mode-bit
//!   interpretation of a file's owner/group bits needs.
//!
//! # C fidelity — the `access` slot is `access(2)`, not the mode bits
//!
//! `prim_file`'s path-based `read_file_info` does **not** derive the slot from `st_mode`: it asks
//! the kernel twice (`efile_read_info`, `erts/emulator/nifs/unix/unix_prim_file.c:754-762`):
//!
//! ```c
//!     result->access = EFILE_ACCESS_NONE;
//!     if(access((const char*)path->data, R_OK) == 0) {
//!         result->access |= EFILE_ACCESS_READ;
//!     }
//!     if(access((const char*)path->data, W_OK) == 0) {
//!         result->access |= EFILE_ACCESS_WRITE;
//!     }
//! ```
//!
//! so [`path_access`] reproduces it as two **independent** [`access`] probes. Note that in that C
//! code *every* non-zero `access(2)` result means "the bit is clear" — a missing path, a denied
//! permission and a read-only filesystem are the same answer. [`access`] mirrors that
//! ("refusal ⇒ `false`"), but only for the errnos that really are answers to the question; a
//! failure of the syscall itself stays an error rather than silently becoming "no access".
//!
//! The *handle*-based sibling, `check_access` (`:771-801`), has no `access(2)` at all — it reads
//! the mode bits of the triple the process actually belongs to:
//!
//! ```c
//!     if(st->st_uid == geteuid()) {
//!         if(st->st_mode & S_IRUSR) ret |= EFILE_ACCESS_READ;
//!         if(st->st_mode & S_IWUSR) ret |= EFILE_ACCESS_WRITE;
//!         return ret;
//!     }
//!     if(st->st_gid == getegid()) { /* the group triple */ }
//!     /* otherwise the other triple */
//! ```
//!
//! which is why the effective ids are needed as well: [`process_credentials`] supplies the
//! `geteuid()`/`getegid()` those two comparisons are made against.
//!
//! # `beam_rt` adoption contract
//!
//! `beam_rt`'s native `file`/`filelib` floor (`crates/beam_rt/src/native/prim.rs`) is the
//! consumer: the `#file_info{}` `access` slot comes from [`path_access`] and the `uid`/`gid`
//! slot of a path whose owner has to be judged comes with [`process_credentials`]. Nothing else
//! in the tree needs these helpers. [`try_access`] is deliberately the *general* primitive — an
//! errno-preserving `access(2)` — so the reference's `{spawn_executable, Path}` check (which
//! turns `access(Path, X_OK)`'s errno into the `error:enoent`/`error:eacces` raise,
//! `sys_drivers.c:521`) can share this wrapper instead of holding its own `unsafe` block.
//!
//! # Safety
//!
//! `unsafe` appears **only** in the syscall wrappers — the `access(2)` call in [`try_access`] and
//! the `geteuid(2)`/`getegid(2)` pair in [`process_credentials`] — each with an inline
//! `// SAFETY:` rationale. Every other item here is safe Rust, and no caller ever has to uphold a
//! safety contract.

use std::ffi::CString;
use std::io::Error as IoError;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use beam_common::{BeamError, Result};

/// The `mode` argument of `access(2)` — the four `<unistd.h>` checks.
///
/// The names are the C macros minus their suffix (`R_OK` → [`AccessMode::Read`]); the numeric
/// values are taken from `libc` so they are the platform's own, never a re-spelled constant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// `F_OK` — the path exists; no permission bit is examined.
    Exists,
    /// `R_OK` — the process may read the path.
    Read,
    /// `W_OK` — the process may write the path.
    Write,
    /// `X_OK` — the process may execute the path (a file) or search it (a directory).
    Execute,
}

impl AccessMode {
    /// All four modes, in the C's `F_OK`, `X_OK`, `W_OK`, `R_OK` order.
    pub const ALL: [AccessMode; 4] = [
        AccessMode::Exists,
        AccessMode::Read,
        AccessMode::Write,
        AccessMode::Execute,
    ];

    /// The `<unistd.h>` constant `access(2)` takes for this mode.
    ///
    /// `F_OK` = 0, `X_OK` = 1, `W_OK` = 2, `R_OK` = 4 — the POSIX values, read from `libc`
    /// rather than spelled out here.
    pub const fn libc_mode(self) -> libc::c_int {
        match self {
            AccessMode::Exists => libc::F_OK,
            AccessMode::Read => libc::R_OK,
            AccessMode::Write => libc::W_OK,
            AccessMode::Execute => libc::X_OK,
        }
    }
}

/// The raw `access(2)` check, errno-preserving: `Ok(())` iff the kernel grants `mode` for `path`.
///
/// This is the crate's one `access(2)` call site, and it is the form a caller needs when the
/// **reason** matters: the failing errno comes back untouched inside [`BeamError::Io`], so the
/// oracle's own failure shape can be reproduced — the reference's `{spawn_executable, Path}`
/// check turns `access(Path, X_OK)`'s errno into the `error:enoent`/`error:eacces` raise
/// (`sys_drivers.c:521`). A caller that only wants the yes/no answer uses [`access`].
///
/// # Errors
///
/// * [`BeamError::Io`] carrying `access(2)`'s `errno` whenever the check is refused — including
///   the refusals that are ordinary answers to the question (`EACCES`, `ENOENT`, …);
/// * [`BeamError::BadArg`] for a `path` holding an interior NUL byte: there is no C string to
///   hand the kernel, the same `badarg` fault the emulator's own path wrappers raise.
///
/// A wait interrupted by a signal (`EINTR`) is retried, so a signal can never turn a legitimate
/// answer into a spurious failure — the same `EINTR` tolerance [`Poller`](crate::poller::Poller)
/// gives `poll(2)`.
pub fn try_access(path: &Path, mode: AccessMode) -> Result<()> {
    let name = path_name(path)?;
    loop {
        // SAFETY: `name` is a NUL-terminated C string that outlives the call and `mode` is one of
        // the four `<unistd.h>` constants, so `access(2)` reads a valid path and a valid mode.
        // The call neither retains the pointer nor writes through it; a failure only sets `errno`,
        // which is read back immediately (before any other libc call can overwrite it).
        if unsafe { libc::access(name.as_ptr(), mode.libc_mode()) } == 0 {
            return Ok(());
        }
        let err = IoError::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(crate::error::io(err));
    }
}

/// Whether this process has `mode` access to `path` — the boolean form of [`try_access`].
///
/// This asks the question the `#file_info{}` `access` slot asks (`efile_read_info`,
/// `unix_prim_file.c:754-762`), so a **refusal** is `Ok(false)`, never an error: a missing path
/// component, a path with a non-directory component, a symlink loop, an over-long path, a denied
/// permission, a read-only filesystem, a file being executed — every errno that is an answer to
/// "may I?" rather than a failure of the syscall. A failure of the syscall itself (out of kernel
/// memory, an I/O error, a mode the kernel rejects) is returned as [`BeamError::Io`] instead of
/// being silently folded into "no access", which would be a *wrong* answer.
///
/// # Errors
///
/// [`BeamError::BadArg`] for an interior-NUL path; [`BeamError::Io`] for an `access(2)` failure
/// that is not a refusal (see above).
pub fn access(path: &Path, mode: AccessMode) -> Result<bool> {
    match try_access(path, mode) {
        Ok(()) => Ok(true),
        Err(BeamError::Io(err)) if is_refusal(err.raw_os_error()) => Ok(false),
        Err(err) => Err(err),
    }
}

/// The `#file_info{}` `access` slot for `path`: may this process **read** it, and may it
/// **write** it?
///
/// The pair is exactly the reference's two independent probes (`efile_read_info`,
/// `unix_prim_file.c:754-762`), each becoming one of the `read`/`write` bits the runtime renders
/// as `read_write`/`read`/`write`/`none`. The probes are independent — a writable-but-unreadable
/// path answers `(false, true)` — and each is [`access`], so a refusal is `false`, never an
/// error.
///
/// # Errors
///
/// As [`access`]: [`BeamError::BadArg`] for an interior-NUL path, [`BeamError::Io`] for a syscall
/// failure that is not a refusal.
pub fn path_access(path: &Path) -> Result<(bool, bool)> {
    let readable = access(path, AccessMode::Read)?;
    let writable = access(path, AccessMode::Write)?;
    Ok((readable, writable))
}

/// The process's **effective** user and group id (`geteuid(2)`, `getegid(2)`).
///
/// These are the two ids the reference's handle-based `check_access` (`unix_prim_file.c:771-801`)
/// compares a `stat` result against to pick which of the owner/group/other mode triples decides
/// read and write access. The *path*-based slot uses [`path_access`] instead, but a caller that
/// holds only a descriptor, or only a mode word, has no `access(2)` to ask and needs the
/// credentials. The values are the **effective** ids, as the C uses: a set-uid process is judged
/// by the ids it actually runs with.
///
/// `geteuid(2)`/`getegid(2)` take no arguments and cannot fail (POSIX defines them as always
/// succeeding), so this always returns `Ok`; the [`Result`] keeps the uniform fallible signature
/// of this module so a caller can `?` it alongside the other helpers.
pub fn process_credentials() -> Result<(u32, u32)> {
    // SAFETY: `geteuid` takes no arguments, cannot fail, and only reads the calling process's own
    // credentials; there is no precondition for the caller to uphold.
    let euid = unsafe { libc::geteuid() };
    // SAFETY: as above, for `getegid`.
    let egid = unsafe { libc::getegid() };
    Ok((euid, egid))
}

/// The `access(2)` errnos that answer "no" rather than "the call failed".
///
/// The reference treats **every** non-zero `access(2)` result as a cleared bit
/// (`efile_read_info`, `unix_prim_file.c:754-762`), but not every errno is an answer to the
/// question posed: folding an out-of-memory or an I/O failure into "no access" would be exactly
/// the silently-wrong answer the workspace error strategy forbids. These eight are refusals —
/// each says the path is not accessible for a reason the check itself asked about.
const REFUSAL_ERRNOS: [i32; 8] = [
    libc::EACCES,       // permission denied (an ACL or a mode bit said no)
    libc::ENOENT,       // no such file or directory (or a dangling symbolic link)
    libc::ENOTDIR,      // a component of the path is not a directory
    libc::ELOOP,        // too many levels of symbolic links
    libc::ENAMETOOLONG, // the path cannot be resolved: it is too long
    libc::EPERM,        // e.g. the write check against a read-only mount
    libc::EROFS,        // the write check against a read-only file system
    libc::ETXTBSY,      // the write check against a file that is being executed
];

/// Whether `errno` is one of the refusals [`access`] folds into `false`.
fn is_refusal(errno: Option<i32>) -> bool {
    matches!(errno, Some(code) if REFUSAL_ERRNOS.contains(&code))
}

/// `path` as a NUL-terminated C string, or [`BeamError::BadArg`] for an interior NUL byte.
fn path_name(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| crate::error::badarg())
}

#[cfg(test)]
mod tests {
    //! In-module tests: the mode→constant table, the refusal/error split, the interior-NUL fault
    //! and fixture sweeps over a real file, a real directory and a mode-`000` file — plus an
    //! `erl`-oracle cross-check of [`path_access`] and [`process_credentials`] against
    //! `file:read_file_info/1` (loudly skipped when `erl` is not on `PATH`).
    //!
    //! The permission fixtures are written to be deterministic for **any** uid, including a
    //! container running as `uid 0`: where `access(2)` legitimately answers differently for a
    //! privileged process (a mode-`000` file) the sub-case is skipped loudly rather than asserted;
    //! where POSIX fixes the answer for every uid (the execute bit) it is asserted unconditionally.

    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// The bounded wait for the `erl` oracle, so a wedged runtime cannot hang the suite.
    const ERL_TIMEOUT: Duration = Duration::from_secs(30);

    // -------------------------------------------------------------------------------------------
    // Fixtures
    // -------------------------------------------------------------------------------------------

    /// A fresh, empty scratch directory under [`std::env::temp_dir`], removed on drop.
    ///
    /// The name mixes the stem, the process id, a per-call counter and the wall clock so parallel
    /// test binaries never collide, and any directory already at that path is removed first — the
    /// fixtures are created with explicit permission bits, so a stale one would be a stale mode.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(stem: &str) -> Scratch {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "beam_io-fs_access-{stem}-{}-{n}-{nanos}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create the scratch directory");
            Scratch(dir)
        }

        /// A path inside the scratch directory (nothing is created for it).
        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Deleting needs write permission on the *directory*, never on the file, so even the
            // mode-`000` fixture is removable here.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Create `path` holding `bytes`, with the permission bits `mode`.
    fn write_file(path: &Path, mode: u32, bytes: &[u8]) {
        fs::write(path, bytes).expect("write the fixture file");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set the fixture mode");
    }

    /// Print a loud skip banner (never a silent pass), with the reason.
    fn loud_skip(reason: &str) {
        println!("================================================================");
        println!("SKIP: {reason}");
        println!("================================================================");
    }

    /// Is this process privileged (`euid == 0`)? Such a process's `access(2)` ignores the mode
    /// bits through `CAP_DAC_OVERRIDE`, so a mode-based refusal cannot be asserted for it.
    fn running_as_root() -> bool {
        process_credentials().expect("the credentials are readable").0 == 0
    }

    /// The `#file_info{}` `access` atom for a `(read, write)` pair — the oracle's
    /// `EFILE_ACCESS_*` rendering (`read`, `write`, `read_write`, `none`).
    fn access_word((readable, writable): (bool, bool)) -> &'static str {
        match (readable, writable) {
            (true, true) => "read_write",
            (true, false) => "read",
            (false, true) => "write",
            (false, false) => "none",
        }
    }

    // -------------------------------------------------------------------------------------------
    // The mode table
    // -------------------------------------------------------------------------------------------

    #[test]
    fn the_four_modes_are_the_unistd_constants() {
        assert_eq!(AccessMode::Exists.libc_mode(), libc::F_OK);
        assert_eq!(AccessMode::Read.libc_mode(), libc::R_OK);
        assert_eq!(AccessMode::Write.libc_mode(), libc::W_OK);
        assert_eq!(AccessMode::Execute.libc_mode(), libc::X_OK);

        // ... and the values themselves are the POSIX-fixed ones (`F_OK` 0, `X_OK` 1, `W_OK` 2,
        // `R_OK` 4), each appearing exactly once in `ALL`.
        assert_eq!(
            AccessMode::ALL.map(AccessMode::libc_mode),
            [libc::F_OK, libc::R_OK, libc::W_OK, libc::X_OK]
        );
        assert_eq!(AccessMode::Exists.libc_mode(), 0);
        assert_eq!(AccessMode::Execute.libc_mode(), 1);
        assert_eq!(AccessMode::Write.libc_mode(), 2);
        assert_eq!(AccessMode::Read.libc_mode(), 4);
    }

    // -------------------------------------------------------------------------------------------
    // Real fixtures: a file, a directory, a missing path, the execute bit, a denied mode
    // -------------------------------------------------------------------------------------------

    #[test]
    fn an_owned_file_with_its_rw_bits_is_readable_and_writable() {
        let scratch = Scratch::new("rw");
        let file = scratch.child("rw.txt");
        write_file(&file, 0o644, b"read_write\n");

        assert!(access(&file, AccessMode::Exists).expect("a refusal is not an error"));
        assert!(access(&file, AccessMode::Read).expect("a refusal is not an error"));
        assert!(access(&file, AccessMode::Write).expect("a refusal is not an error"));
        assert_eq!(path_access(&file).expect("path_access"), (true, true));
    }

    #[test]
    fn an_owned_directory_with_its_rw_bits_is_readable_and_writable() {
        let scratch = Scratch::new("dir");
        let dir = scratch.child("sub");
        fs::create_dir_all(&dir).expect("create the directory fixture");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))
            .expect("set the directory mode");

        // The owner triple grants both bits (an unprivileged owner reads them directly; a
        // privileged process gets them through `CAP_DAC_OVERRIDE`), so this holds for every uid.
        assert!(access(&dir, AccessMode::Read).expect("a refusal is not an error"));
        assert!(access(&dir, AccessMode::Write).expect("a refusal is not an error"));
        assert_eq!(path_access(&dir).expect("path_access"), (true, true));
    }

    #[test]
    fn a_missing_path_is_a_refusal_and_try_access_keeps_its_errno() {
        let scratch = Scratch::new("missing");
        let missing = scratch.child("not-there");

        for mode in AccessMode::ALL {
            assert!(
                !access(&missing, mode).expect("a refusal is not an error"),
                "{mode:?} on a missing path is a refusal, not an error"
            );
        }
        assert_eq!(path_access(&missing).expect("path_access"), (false, false));

        // The errno-preserving form keeps `enoent` — what a caller that must reproduce the
        // oracle's `error:enoent` raise (the reference's `access(Path, X_OK)` spawn check) needs.
        match try_access(&missing, AccessMode::Exists) {
            Err(BeamError::Io(err)) => assert_eq!(err.raw_os_error(), Some(libc::ENOENT)),
            other => panic!("expected the raw enoent failure, got {other:?}"),
        }
    }

    #[test]
    fn the_execute_bit_is_what_decides_x_ok() {
        let scratch = Scratch::new("exec");
        let program = scratch.child("program");
        write_file(&program, 0o755, b"#!/bin/sh\nexit 0\n");
        assert!(access(&program, AccessMode::Execute).expect("a refusal is not an error"));
        assert_eq!(path_access(&program).expect("path_access"), (true, true));
    }

    #[test]
    fn a_file_without_any_execute_bit_is_not_executable() {
        let scratch = Scratch::new("noexec");
        let plain = scratch.child("plain");
        write_file(&plain, 0o644, b"not a program\n");

        // POSIX fixes this for every uid: even with appropriate privileges, `X_OK` needs at least
        // one execute bit somewhere (`CAP_DAC_OVERRIDE` alone is not enough), so unlike the
        // read/write checks this assertion is safe under a uid-0 container too.
        assert!(
            !access(&plain, AccessMode::Execute).expect("a refusal is not an error"),
            "a file with no execute bit is not executable, for any uid"
        );
    }

    #[test]
    fn access_is_denied_by_the_mode_bits_for_an_unprivileged_process() {
        let scratch = Scratch::new("denied");
        let closed = scratch.child("closed.txt");
        write_file(&closed, 0o000, b"");

        // Existence is never a permission question, so this holds whatever the uid.
        assert!(access(&closed, AccessMode::Exists).expect("a refusal is not an error"));

        if running_as_root() {
            loud_skip(
                "euid is 0, so access(2) answers through CAP_DAC_OVERRIDE and ignores the \
                 mode-000 bits: the read/write refusal cannot be asserted here",
            );
            return;
        }
        assert!(!access(&closed, AccessMode::Read).expect("a refusal is not an error"));
        assert!(!access(&closed, AccessMode::Write).expect("a refusal is not an error"));
        assert_eq!(path_access(&closed).expect("path_access"), (false, false));

        // ... and the reason is the permission, which the raw form preserves.
        match try_access(&closed, AccessMode::Read) {
            Err(BeamError::Io(err)) => assert_eq!(err.raw_os_error(), Some(libc::EACCES)),
            other => panic!("expected the raw eacces failure, got {other:?}"),
        }
    }

    #[test]
    fn an_interior_nul_is_badarg() {
        // Every unix path is bytes; a NUL is not a byte a C string can carry, so there is nothing
        // to ask the kernel and the fault is the oracle's `badarg` (not a panic and not `false`).
        let nul = Path::new("no\0such");
        assert!(matches!(
            try_access(nul, AccessMode::Exists),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            access(nul, AccessMode::Write),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(path_access(nul), Err(BeamError::BadArg)));
    }

    #[test]
    fn process_credentials_are_the_owner_of_a_file_this_process_creates() {
        let scratch = Scratch::new("creds");
        let file = scratch.child("owned");
        write_file(&file, 0o644, b"");

        // A freshly created file is owned by its creator's effective ids (POSIX; `$TMPDIR` is a
        // local file system here), so the credentials are observable without a second syscall.
        let meta = fs::metadata(&file).expect("stat the fixture");
        let (euid, egid) = process_credentials().expect("the credentials are readable");
        assert_eq!(euid, meta.uid(), "the effective uid owns a file it just created");
        assert_eq!(egid, meta.gid(), "the effective gid owns a file it just created");
    }

    // -------------------------------------------------------------------------------------------
    // The `erl` oracle cross-check
    // -------------------------------------------------------------------------------------------

    /// An Erlang string literal for an ASCII `path`, or `None` if it needs escaping the oracle
    /// expression below does not perform (the scratch path is ASCII by construction, so this is a
    /// safety net rather than an expected branch).
    fn erl_string(path: &Path) -> Option<String> {
        let text = path.to_str()?;
        (!text.contains(['\\', '"'])).then(|| format!("\"{text}\""))
    }

    /// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after a
    /// loud skip message — when `erl` cannot be spawned or does not answer within [`ERL_TIMEOUT`].
    fn erl_stdout(expr: &str) -> Option<String> {
        let code = format!("{expr}, halt(0).");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                Command::new("erl")
                    .arg("-noshell")
                    .arg("-eval")
                    .arg(&code)
                    .output(),
            );
        });

        match rx.recv_timeout(ERL_TIMEOUT) {
            Err(_) => {
                loud_skip(&format!(
                    "the `erl` oracle did not answer within {ERL_TIMEOUT:?}"
                ));
                None
            }
            Ok(Err(err)) => {
                loud_skip(&format!(
                    "could not spawn `erl` ({err}); is Erlang on `PATH`?"
                ));
                None
            }
            Ok(Ok(out)) => {
                assert!(
                    out.status.success(),
                    "the oracle expression failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
        }
    }

    /// The oracle's `#file_info{}` `access`/`uid`/`gid` against [`path_access`] /
    /// [`process_credentials`].
    ///
    /// The oracle's `access` slot *is* `access(2)` (`efile_read_info`, `unix_prim_file.c:754-762`),
    /// so this compares the two implementations of the same probe on three fixtures — a `0644`
    /// file, a `0755` directory and a mode-`000` file. The last one is the interesting case: its
    /// answer legitimately depends on the uid (a uid-0 container is told `read_write`, an
    /// unprivileged process `none`), and the helper must agree with the oracle **either way** —
    /// which is what makes this cross-check uid-robust.
    ///
    /// The record is read by field position, in the order of `lib/kernel/include/file.hrl`:
    /// `{file_info, size, type, access, atime, mtime, ctime, mode, links, major_device,
    /// minor_device, inode, uid, gid}` — the tag at 1, `access` at 4, `uid` at 13, `gid` at 14.
    #[test]
    fn the_oracle_agrees_on_the_access_slot_and_the_credentials() {
        let scratch = Scratch::new("oracle");
        let rw = scratch.child("read_write.txt");
        write_file(&rw, 0o644, b"the oracle cross-check\n");
        let dir = scratch.child("dir");
        fs::create_dir_all(&dir).expect("create the directory fixture");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))
            .expect("set the directory mode");
        let closed = scratch.child("closed.txt");
        write_file(&closed, 0o000, b"");

        let (Some(rw_e), Some(dir_e), Some(closed_e)) =
            (erl_string(&rw), erl_string(&dir), erl_string(&closed))
        else {
            loud_skip("the scratch path is not an ASCII path; skipping the oracle cross-check");
            return;
        };
        let expr = format!(
            "Sh = fun(P) -> {{ok,I}} = file:read_file_info(P), \
               io:format(\"~p|~p|~p|~p~n\", [element(1,I),element(4,I),element(13,I),element(14,I)]) end, \
             Sh({rw_e}), Sh({dir_e}), Sh({closed_e})"
        );
        let Some(stdout) = erl_stdout(&expr) else {
            return;
        };

        let rows: Vec<Vec<&str>> = stdout
            .lines()
            .filter(|line| line.contains('|'))
            .map(|line| line.split('|').collect())
            .collect();
        assert_eq!(rows.len(), 3, "one row per probed path, got: {stdout}");
        for row in &rows {
            assert_eq!(row.len(), 4, "tag|access|uid|gid, got: {row:?}");
            assert_eq!(
                row[0], "file_info",
                "the oracle answered with another record: {stdout}"
            );
        }

        // The `access` slot, atom for atom: the reference computes it with the very same
        // `access(2)` probes `path_access` runs.
        let ours = [path_access(&rw), path_access(&dir), path_access(&closed)];
        for (row, access) in rows.iter().zip(ours) {
            let access = access.expect("a refusal is not an error");
            assert_eq!(
                row[1],
                access_word(access),
                "path_access gave {access:?}, the oracle said {:?}",
                row[1]
            );
        }

        // The credentials: the fixtures were created by *this* process, so the oracle's
        // `uid`/`gid` (the file's `st_uid`/`st_gid`) are this process's `geteuid`/`getegid`.
        let (euid, egid) = process_credentials().expect("the credentials are readable");
        assert_eq!(
            rows[0][2],
            euid.to_string(),
            "the oracle's uid is this process's euid"
        );
        assert_eq!(
            rows[0][3],
            egid.to_string(),
            "the oracle's gid is this process's egid"
        );
    }
}
