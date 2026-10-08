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

//! Read-only **target adapter**: stage an external repository subtree into a private
//! scratch directory under `$TMPDIR`, build/run it there, and clean it up on drop.
//!
//! The external benchmark targets (Elixir, Cowboy, Khepri, EMQX, …) are configured as
//! **read-only** foreign repositories: the driver may build and run them but must
//! *never* write into them. Several of those builds are nevertheless in-tree by nature
//! — `make`, `rebar3`, `dotnet` and friends drop object files, `_build`/`ebin` output,
//! relocatable `_rel` trees and generated sources next to (or inside) the sources, and
//! some of them even mutate tracked files. Building in place would therefore violate
//! the read-only contract.
//!
//! This module is the adapter that makes those workloads safe:
//!
//! 1. [`ScratchDir::new`] creates a uniquely named, private working directory under
//!    `std::env::temp_dir()` (`$TMPDIR`).
//! 2. [`copy_tree`] (a whole subtree, e.g. a checkout minus `.git`/build output) or
//!    [`copy_files`] (a handful of individual files) stages the read-only sources into
//!    that directory, **without touching the originals**.
//! 3. The workload builds/runs against the staged copy.
//! 4. [`ScratchDir`]'s [`Drop`] removes the whole tree again — even on the error path.
//!
//! Everything here is pure `std` (no `libc`, no `unsafe`, no new crates), so it stays
//! inside the crate's dependency and `#![forbid(unsafe_code)]` budget. Copying uses
//! [`std::fs::copy`], which preserves the file mode — crucial because build scripts
//! (`configure`, `*.sh`, `rebar3`, wrapper `erlc`) must stay executable.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-global sequence number that, together with the pid and a nanosecond
/// timestamp, makes every [`ScratchDir`] name unique — even for two instances created
/// in the same nanosecond by different threads of the same process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A private, self-cleaning working directory under [`std::env::temp_dir()`].
///
/// Created by [`ScratchDir::new`] and removed (best-effort) when dropped, so a workload
/// can stage a read-only external repository, build there, and let RAII clean up after
/// it — including when the build fails and the caller returns early with `?`.
///
/// ```no_run
/// use bench::scratch::ScratchDir;
/// use std::path::Path;
///
/// let dir = ScratchDir::new("cowboy").unwrap();
/// bench::scratch::copy_tree(Path::new("/home/bill/Source/cowboy"), dir.path(), &[".git"]).unwrap();
/// // ... build/run inside `dir.path()` ...
/// // `dir` is removed here, leaving the source checkout untouched.
/// ```
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Creates a fresh scratch directory named `bench-scratch-<tag>-<pid>-<seq>-<nanos>`.
    ///
    /// The name embeds the process id, a process-global atomic counter and a nanosecond
    /// timestamp so that concurrent workloads (and the same workload under several
    /// runtimes) never collide. The directory is created with
    /// [`std::fs::create_dir_all`], so `$TMPDIR` must be writable.
    ///
    /// Errors are surfaced unchanged (e.g. a read-only `$TMPDIR` yields
    /// [`std::io::ErrorKind::PermissionDenied`]).
    pub fn new(tag: &str) -> io::Result<ScratchDir> {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!(
            "bench-scratch-{}-{}-{}-{}",
            sanitize_tag(tag),
            std::process::id(),
            seq,
            nanos
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path)?;
        Ok(ScratchDir { path })
    }

    /// The scratch directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    /// Removes the whole tree, ignoring every error.
    ///
    /// Cleanup must never turn a successful benchmark into a failure, and a missing or
    /// partially-removed directory is harmless: `$TMPDIR` is reclaimable by the OS. All
    /// *substantive* errors are reported by the operations that created the tree, not
    /// here.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Recursively copies the **contents** of `src` into `dst`.
///
/// `dst` and every subdirectory are created as needed; existing files are overwritten
/// (as [`std::fs::copy`] does) but nothing in `dst` is deleted first. File modes are
/// preserved (`std::fs::copy` copies the permission bits), so executable build scripts
/// stay executable. On Unix, symlinks are recreated as symlinks (they are *not*
/// followed, which also prevents symlink loops); on other platforms a symlink to a
/// regular file is copied by content and any other symlink is skipped.
///
/// Any entry whose final path component (file name) equals one of `skip_names` is
/// skipped, at *every* depth — e.g. `.git`, `_build`, `_rel`, `deps`, `ebin`,
/// `.erlang.mk`. Filesystem node kinds that cannot be copied with `std` (FIFOs,
/// sockets, device nodes) are skipped.
///
/// A missing or unreadable `src` is **not** swallowed: [`std::fs::read_dir`] fails and
/// the error (e.g. [`std::io::ErrorKind::NotFound`]) is returned before `dst` is
/// created, so a mistyped target-repo path is reported loudly.
pub fn copy_tree(src: &Path, dst: &Path, skip_names: &[&str]) -> io::Result<()> {
    // Read the source *first*: this surfaces `NotFound`/`PermissionDenied` for `src`
    // without leaving a half-created `dst` behind.
    let entries: Vec<fs::DirEntry> = fs::read_dir(src)?.collect::<io::Result<Vec<_>>>()?;
    fs::create_dir_all(dst)?;
    for entry in entries {
        if is_skipped(&entry, skip_names) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&from, &to, skip_names)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        } else if file_type.is_symlink() {
            copy_symlink(&from, &to)?;
        }
        // Anything else (FIFO, socket, device node, …) cannot be staged with `std`.
    }
    // Mirror the source directory's mode *after* recursing, so a read-only source mode
    // never blocks writing the files inside it.
    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(dst, meta.permissions());
    }
    Ok(())
}

/// Copies individual files into `dst`, returning how many were actually copied.
///
/// `dst` is created (recursively) if needed; each file keeps its own file name. File
/// modes are preserved exactly as in [`copy_tree`].
///
/// A listed path that does **not exist** or is not a regular file (e.g. a directory) is
/// skipped and *not* counted — that is the "actually copied" semantics: callers that
/// require every file must compare the returned count against `files.len()` (or pass a
/// list they know exists). This leniency exists solely for genuinely optional files;
/// every other failure (permission denied, a full disk, an unwritable `dst`, …) is
/// propagated unchanged rather than swallowed. Symlinks are followed (`fs::metadata`)
/// and copied by content.
pub fn copy_files(files: &[PathBuf], dst: &Path) -> io::Result<usize> {
    fs::create_dir_all(dst)?;
    let mut copied = 0usize;
    for file in files {
        // A path with no final component (`/`, `..`) cannot be copied as a named file.
        let Some(name) = file.file_name() else {
            continue;
        };
        let meta = match fs::metadata(file) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if !meta.is_file() {
            continue;
        }
        fs::copy(file, dst.join(name))?;
        copied += 1;
    }
    Ok(copied)
}

/// Whether `entry`'s file name is listed in `skip_names`.
///
/// File names that are not valid UTF-8 can never equal a `&str` skip name, so they are
/// (correctly) not skipped.
fn is_skipped(entry: &fs::DirEntry, skip_names: &[&str]) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| skip_names.contains(&name))
}

/// Recreates a symbolic link at `dst` pointing at whatever `src` points to.
#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    std::os::unix::fs::symlink(target, dst)
}

/// Non-Unix fallback: there is no portable "create symlink" in `std`, so copy the link's
/// resolved content when it is a regular file and otherwise skip it (see [`copy_tree`]).
#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    if fs::metadata(src).map(|m| m.is_file()).unwrap_or(false) {
        fs::copy(src, dst).map(|_| ())?;
    }
    Ok(())
}

/// Turns an arbitrary tag into a filesystem-safe path component.
///
/// Only ASCII alphanumerics, `-` and `_` are kept (everything else becomes `-`); an
/// empty tag yields `scratch`, so the directory name is always non-empty.
fn sanitize_tag(tag: &str) -> String {
    let cleaned: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "scratch".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `contents` to `path`, creating parent directories as needed.
    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write test file");
    }

    #[test]
    fn new_creates_a_directory_under_temp_dir() {
        let dir = ScratchDir::new("unit").expect("create scratch dir");
        assert!(dir.path().is_dir(), "path must be a directory");
        assert!(
            dir.path().starts_with(std::env::temp_dir()),
            "scratch must live under $TMPDIR, got {:?}",
            dir.path()
        );
    }

    #[test]
    fn paths_are_unique_per_instance_even_with_the_same_tag() {
        let a = ScratchDir::new("dup").expect("create a");
        let b = ScratchDir::new("dup").expect("create b");
        assert_ne!(a.path(), b.path());
        assert!(a.path().is_dir() && b.path().is_dir());
    }

    #[test]
    fn tag_is_sanitized_and_an_empty_tag_still_yields_a_name() {
        let dir = ScratchDir::new("weird/tag with spaces").expect("create");
        let name = dir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name")
            .to_string();
        assert!(
            name.starts_with("bench-scratch-weird-tag-with-spaces-"),
            "{name}"
        );
        let empty = ScratchDir::new("").expect("create");
        let empty_name = empty
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name")
            .to_string();
        assert!(
            empty_name.starts_with("bench-scratch-scratch-"),
            "{empty_name}"
        );
    }

    #[test]
    fn drop_removes_the_whole_tree() {
        let dir = ScratchDir::new("drop").expect("create");
        let path = dir.path().to_path_buf();
        write(&path.join("nested/deeper/file.txt"), "hi");
        assert!(path.join("nested/deeper/file.txt").is_file());
        drop(dir);
        assert!(!path.exists(), "scratch tree must be removed on drop");
    }

    #[test]
    fn copy_tree_copies_contents_and_skips_named_entries_at_every_depth() {
        let src = ScratchDir::new("copy-src").expect("src");
        let dst = ScratchDir::new("copy-dst").expect("dst");
        write(&src.path().join("top.txt"), "top");
        write(&src.path().join("sub/nested.txt"), "nested");
        write(&src.path().join(".git/config"), "git-meta");
        write(&src.path().join("sub/ebin/thing.beam"), "beam");

        copy_tree(src.path(), dst.path(), &[".git", "ebin"]).expect("copy tree");

        assert_eq!(
            fs::read_to_string(dst.path().join("top.txt")).expect("read top"),
            "top"
        );
        assert_eq!(
            fs::read_to_string(dst.path().join("sub/nested.txt")).expect("read nested"),
            "nested"
        );
        assert!(!dst.path().join(".git").exists(), ".git must be skipped");
        assert!(
            !dst.path().join("sub/ebin").exists(),
            "a nested skipped name must be skipped too"
        );
    }

    #[test]
    fn copy_tree_of_a_missing_source_is_not_found_and_creates_nothing() {
        let dst = ScratchDir::new("missing-dst").expect("dst");
        let missing_src = dst.path().join("does-not-exist");
        let out = dst.path().join("out");
        let err = copy_tree(&missing_src, &out, &[]).expect_err("missing src must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            !out.exists(),
            "a failed copy must not leave a half-created destination"
        );
    }

    #[test]
    fn copy_tree_overwrites_existing_files_and_never_deletes_extras() {
        let src = ScratchDir::new("overwrite-src").expect("src");
        let dst = ScratchDir::new("overwrite-dst").expect("dst");
        write(&src.path().join("a.txt"), "new");
        write(&dst.path().join("a.txt"), "old");
        write(&dst.path().join("keep.txt"), "keep");
        copy_tree(src.path(), dst.path(), &[]).expect("copy tree");
        assert_eq!(
            fs::read_to_string(dst.path().join("a.txt")).expect("read a"),
            "new"
        );
        assert!(dst.path().join("keep.txt").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let src = ScratchDir::new("mode-src").expect("src");
        let dst = ScratchDir::new("mode-dst").expect("dst");
        let script = src.path().join("run.sh");
        write(&script, "#!/bin/sh\necho hello\n");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod 755");

        copy_tree(src.path(), dst.path(), &[]).expect("copy tree");

        let mode = fs::metadata(dst.path().join("run.sh"))
            .expect("stat copy")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits must survive the copy");
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_recreates_symlinks_without_following_them() {
        let src = ScratchDir::new("link-src").expect("src");
        let dst = ScratchDir::new("link-dst").expect("dst");
        write(&src.path().join("real.txt"), "real");
        std::os::unix::fs::symlink("real.txt", src.path().join("link.txt")).expect("symlink");

        copy_tree(src.path(), dst.path(), &[]).expect("copy tree");

        let meta = fs::symlink_metadata(dst.path().join("link.txt")).expect("stat link");
        assert!(meta.file_type().is_symlink(), "symlink must stay a symlink");
        assert_eq!(
            fs::read_to_string(dst.path().join("link.txt")).expect("follow link"),
            "real"
        );
    }

    #[test]
    fn copy_files_copies_each_file_and_creates_dst() {
        let src = ScratchDir::new("files-src").expect("src");
        write(&src.path().join("a.txt"), "a");
        write(&src.path().join("b.txt"), "b");
        let files = vec![src.path().join("a.txt"), src.path().join("b.txt")];
        let dst = src.path().join("staged/out");
        assert!(!dst.exists(), "dst must not exist yet");

        let copied = copy_files(&files, &dst).expect("copy files");

        assert_eq!(copied, 2);
        assert!(dst.is_dir(), "copy_files must create dst");
        assert_eq!(fs::read_to_string(dst.join("a.txt")).expect("read a"), "a");
        assert_eq!(fs::read_to_string(dst.join("b.txt")).expect("read b"), "b");
    }

    #[test]
    fn copy_files_counts_only_what_it_actually_copied() {
        let src = ScratchDir::new("files-partial").expect("src");
        write(&src.path().join("present.txt"), "x");
        let files = vec![
            src.path().join("present.txt"),
            src.path().join("absent.txt"),
            src.path().join("sub"), // a directory is not a file
        ];
        let dst = src.path().join("out");

        let copied = copy_files(&files, &dst).expect("copy files");

        assert_eq!(copied, 1, "only the regular file is copied");
        assert!(dst.join("present.txt").is_file());
        assert!(!dst.join("absent.txt").exists());
        assert!(!dst.join("sub").exists());
    }

    #[test]
    fn copy_files_of_an_empty_list_still_creates_dst() {
        let dst = ScratchDir::new("files-empty").expect("dst");
        let out = dst.path().join("out");
        assert_eq!(copy_files(&[], &out).expect("copy"), 0);
        assert!(out.is_dir());
    }

    #[test]
    fn sanitize_tag_keeps_only_safe_characters() {
        assert_eq!(sanitize_tag("cowboy"), "cowboy");
        assert_eq!(sanitize_tag("a/b c"), "a-b-c");
        assert_eq!(sanitize_tag(""), "scratch");
        assert_eq!(sanitize_tag("../../etc"), "------etc");
    }
}
