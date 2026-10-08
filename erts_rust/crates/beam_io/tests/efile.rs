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

//! Integration tests for the `efile` driver's **file operations** — the open modes and the
//! position/sync/truncate verbs added to [`beam_io::drivers::EfileDriver`], plus regression
//! coverage for the read-only `open`/`close`/`path`/`eof`/framing family it already had.
//!
//! The driver is the Rust counterpart of the reference's `efile` layer (the `open(2)` flag
//! translation is `efile_translate_modelist` + `get_flags`,
//! `erts/emulator/nifs/common/prim_file_nif.c:500-539` /
//! `erts/emulator/nifs/unix/unix_prim_file.c:142-171`; `truncate/1` is `ftruncate(fd,
//! lseek(fd, 0, SEEK_CUR))`, `unix_prim_file.c:693-704`). Every suite drives **real files**
//! through the real reactor and reads the bytes back from the OS, and
//! `the_file_verbs_and_flags_agree_with_the_erl_oracle` replays the identical operation
//! sequence under the system `erl` and compares the observable outcome field for field.
//! When `erl` is absent that check **skips loudly**, never fails.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_common::BeamError;
use beam_io::prelude::*;
use beam_process::PortId;

/// Wall-clock budget for the `erl` oracle before the cross-check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// A fresh poller.
fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// A unique scratch directory (created), removed by [`Scratch::drop`].
struct Scratch {
    /// The directory this run owns.
    dir: PathBuf,
}

/// The per-process scratch-directory counter.
static SCRATCH_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Scratch {
    /// Creates a fresh, empty scratch directory tagged `tag`.
    fn new(tag: &str) -> Scratch {
        let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "beam_io_efile_{tag}_{}_{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is created");
        Scratch { dir }
    }

    /// The scratch directory itself.
    fn path(&self) -> &Path {
        &self.dir
    }

    /// A path inside the scratch directory (not created).
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The scratch directory as a byte string (the driver's path encoding).
    fn bytes(&self) -> &[u8] {
        self.dir.as_os_str().as_encoded_bytes()
    }

    /// `path` as a byte string.
    fn bytes_of(path: &Path) -> Vec<u8> {
        path.as_os_str().as_encoded_bytes().to_vec()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Opens an unopened `efile` port on a fresh poller/registry pair.
fn open_efile(registry: &mut PortRegistry, poller: &mut Poller) -> PortId {
    registry
        .open(poller, Box::new(EfileDriver::unopened()))
        .expect("the unopened efile port starts")
}

/// Delivers one control command to `id`.
fn ctl(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    verb: &str,
    arg: &[u8],
) -> Result<Vec<u8>, BeamError> {
    registry
        .get_mut(id)
        .expect("the port is live")
        .control(poller, verb, arg)
}

/// Delivers one control command that is expected to succeed.
fn ctl_ok(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    verb: &str,
    arg: &[u8],
) -> Vec<u8> {
    ctl(registry, poller, id, verb, arg)
        .unwrap_or_else(|err| panic!("the `{verb}` control command failed: {err:?}"))
}

/// Writes `data` through the port's output path and asserts the driver took all of it.
fn write_all(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    data: &[u8],
) -> usize {
    let written = registry
        .output(poller, id, data)
        .expect("the driver writes to the file");
    assert_eq!(written, data.len(), "the whole payload is written");
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        0,
        "nothing stalls in the output queue"
    );
    written
}

/// The file's bytes as read straight back from the OS.
fn content(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("the file is readable")
}

/// The BEAM atom the oracle would name for a `BeamError::Io`, or `None` for anything else.
///
/// The driver reports a failed `open(2)` as `BeamError::Io` carrying the errno; the oracle
/// renders the same failure as `{error, <atom>}` (`efile_open` returns the raw `errno`,
/// `unix_prim_file.c:173-226`, and `posix_error_to_tuple` maps it to an atom).
fn errno_atom(err: &BeamError) -> Option<&'static str> {
    let BeamError::Io(io) = err else {
        return None;
    };
    match io.raw_os_error()? {
        libc::EISDIR => Some("eisdir"),
        libc::ENOENT => Some("enoent"),
        libc::EEXIST => Some("eexist"),
        libc::ENOTDIR => Some("enotdir"),
        _ => None,
    }
}

/// Asserts `result` is the `BeamError::Io` carrying `expected` (a POSIX errno).
fn assert_errno(result: Result<Vec<u8>, BeamError>, expected: i32, what: &str) {
    match result {
        Err(ref err) => assert_eq!(
            errno_atom(err),
            Some(match expected {
                libc::EISDIR => "eisdir",
                libc::ENOENT => "enoent",
                libc::EEXIST => "eexist",
                libc::ENOTDIR => "enotdir",
                _ => "unmapped",
            }),
            "`{what}` should have failed with errno {expected}: {err:?}"
        ),
        Ok(reply) => panic!("`{what}` unexpectedly succeeded: {reply:?}"),
    }
}

// ---------------------------------------------------------------------------
// Read-only family regression (open/close/path/eof/framing)
// ---------------------------------------------------------------------------

/// The read-only `open <path>` form is unchanged: it reads a regular file to EOF through the
/// poller's `poll(2)` fallback readiness source, remembers the path and answers `eof`/`path`/
/// `getopts`, and `close` releases the descriptor.
#[test]
fn the_read_only_open_still_reads_a_regular_file_to_eof() {
    let scratch = Scratch::new("read_only");
    let path = scratch.file("payload.txt");
    let payload = b"the quick brown fox\njumps over the lazy dog\n";
    std::fs::write(&path, payload).expect("the fixture file is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    // A bare path (no flag token) is the backwards-compatible read-only open.
    ctl_ok(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&path));
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "path", b""),
        Scratch::bytes_of(&path),
        "the driver remembers the opened path"
    );
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "getopts", b""),
        b"binary",
        "the default framing phase is raw binary"
    );
    assert_eq!(poller.len(), 1, "the regular file is registered with the poller");

    // Pump the reactor until the whole file has arrived; the driver reads a regular file to
    // EOF inside one `ready_input` round and de-registers it.
    let mut input = Vec::new();
    while input.len() < payload.len() {
        let events = poller
            .poll(Some(Duration::from_millis(200)))
            .expect("poll succeeds");
        registry
            .handle_events(&mut poller, &events)
            .expect("events dispatch");
        input.extend(registry.get_mut(id).expect("live").drain_input());
    }
    assert_eq!(input, payload.to_vec());
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "eof", b""),
        b"true",
        "end-of-file is latched"
    );
    assert!(poller.is_empty(), "EOF de-registers the regular file");

    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert!(poller.is_empty(), "`close` deregisters the file");
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "path", b""),
        b"",
        "`close` forgets the path"
    );
    registry.close(&mut poller, id).expect("closes the port");
}

/// A path containing a space is **not** mis-read as a flag list: the whole argument is the
/// path whenever its first token is not entirely made of recognised flag words.
#[test]
fn a_path_containing_a_space_is_still_a_path() {
    let scratch = Scratch::new("space_path");
    let path = scratch.file("with space.txt");
    std::fs::write(&path, b"spaced\n").expect("the fixture file is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    ctl_ok(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&path));
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "path", b""),
        Scratch::bytes_of(&path)
    );
    registry.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// Open modes
// ---------------------------------------------------------------------------

/// `open write <path>` truncates an existing file and writes through the port output queue —
/// the `O_TRUNC | O_WRONLY | O_CREAT` shape (`get_flags`, `unix_prim_file.c:146-147`).
#[test]
fn a_write_mode_open_truncates_and_writes() {
    let scratch = Scratch::new("write_mode");
    let path = scratch.file("write.txt");
    std::fs::write(&path, b"contents that must be truncated").expect("the fixture is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    let mut arg = b"write ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    assert!(
        poller.is_empty(),
        "a write-only open arms no read registration"
    );
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "path", b""),
        Scratch::bytes_of(&path)
    );

    write_all(&mut registry, &mut poller, id, b"new contents");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(content(&path), b"new contents".to_vec());

    // The port is reusable after `close`: a second open on the same port works.
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"second");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(content(&path), b"second".to_vec());

    // The released descriptor really is gone (and was closed exactly once): a further write
    // is a bad argument, not a write through a stale or reused descriptor.
    assert!(matches!(
        registry.output(&mut poller, id, b"x"),
        Err(BeamError::BadArg)
    ));

    registry.close(&mut poller, id).expect("closes the port");
}

/// `read` + `write` is `O_RDWR | O_CREAT` and must **not** truncate
/// (`unix_prim_file.c:148-149`): the write lands at the current offset over the existing bytes.
#[test]
fn a_read_write_open_does_not_truncate() {
    let scratch = Scratch::new("read_write_mode");
    let path = scratch.file("rw.txt");
    std::fs::write(&path, b"012").expect("the fixture is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    let mut arg = b"read,write ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);

    // The read slot is registered (the file is readable) and the write slot is a `dup(2)`.
    assert_eq!(poller.len(), 1, "the read slot is registered");
    write_all(&mut registry, &mut poller, id, b"XY");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(
        content(&path),
        b"XY2".to_vec(),
        "the write overwrote the first two bytes and left the rest"
    );

    registry.close(&mut poller, id).expect("closes the port");
}

/// `append` alone means `O_WRONLY | O_CREAT | O_APPEND` (writing, positioned at end of file),
/// with the `O_TRUNC` of the write-only form cancelled (`unix_prim_file.c:146-157`).
#[test]
fn an_append_open_appends_at_end_of_file() {
    let scratch = Scratch::new("append_mode");
    let path = scratch.file("append.txt");
    std::fs::write(&path, b"XY2").expect("the fixture is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    let mut arg = b"append ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"ZZ");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(
        content(&path),
        b"XY2ZZ".to_vec(),
        "the append did not truncate and landed at the end"
    );

    registry.close(&mut poller, id).expect("closes the port");
}

/// `append`/`exclusive` imply `write`, so `exclusive` alone is `O_WRONLY | O_CREAT | O_EXCL`
/// and refuses an existing path with `EEXIST` — while a fresh path opens.
#[test]
fn an_exclusive_open_refuses_an_existing_path() {
    let scratch = Scratch::new("exclusive");
    let path = scratch.file("exists.txt");
    std::fs::write(&path, b"here").expect("the fixture is written");
    let fresh = scratch.file("fresh.txt");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    let mut arg = b"write,exclusive ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    assert_errno(
        ctl(&mut registry, &mut poller, id, "open", &arg),
        libc::EEXIST,
        "an exclusive open of an existing path",
    );
    assert!(
        poller.is_empty(),
        "a failed open registers no descriptor"
    );

    let mut arg = b"write,exclusive ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&fresh));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"created");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(content(&fresh), b"created".to_vec());

    registry.close(&mut poller, id).expect("closes the port");
}

/// A path whose parent component is a regular file is `ENOTDIR` for a creating open
/// (`O_CREAT` cannot make a directory), and the failure never leaves a descriptor attached.
#[test]
fn a_creating_open_under_a_file_is_enotdir() {
    let scratch = Scratch::new("enotdir");
    let file = scratch.file("plain.txt");
    std::fs::write(&file, b"x").expect("the fixture is written");
    let under = file.join("sub");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    let mut arg = b"write,exclusive ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&under));
    assert_errno(
        ctl(&mut registry, &mut poller, id, "open", &arg),
        libc::ENOTDIR,
        "a creating open under a regular file",
    );
    assert!(poller.is_empty());

    registry.close(&mut poller, id).expect("closes the port");
}

/// `open(2)` on its own opens a directory read-only; the oracle refuses one with `EISDIR`
/// (`open_file_is_dir`, `unix_prim_file.c:206-213`), so the driver must too — and must not
/// leave the descriptor registered.
#[test]
fn opening_a_directory_is_eisdir() {
    let scratch = Scratch::new("eisdir");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    // Both a plain read-only open and a creating read+write open are refused.
    assert_errno(
        ctl(&mut registry, &mut poller, id, "open", scratch.bytes()),
        libc::EISDIR,
        "a read-only open of a directory",
    );
    assert!(poller.is_empty(), "the refused descriptor is closed");
    let mut arg = b"read,write ".to_vec();
    arg.extend_from_slice(scratch.bytes());
    assert_errno(
        ctl(&mut registry, &mut poller, id, "open", &arg),
        libc::EISDIR,
        "a read+write open of a directory",
    );
    assert!(poller.is_empty());

    // A missing path is still the plain `open(2)` failure.
    let missing = scratch.file("nope");
    assert_errno(
        ctl(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&missing)),
        libc::ENOENT,
        "a read-only open of a missing path",
    );

    registry.close(&mut poller, id).expect("closes the port");
}

/// Bad `open` arguments are the typed `BadArg`: an empty path, and a second `open` while a
/// descriptor is still attached.
#[test]
fn invalid_open_arguments_are_bad_arg() {
    let scratch = Scratch::new("bad_open");
    let path = scratch.file("f.txt");
    std::fs::write(&path, b"x").expect("the fixture is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "open", b""),
        Err(BeamError::BadArg)
    ));
    // The empty-path form of the flag grammar (a flag token with nothing after it).
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "open", b"read "),
        Err(BeamError::BadArg)
    ));

    ctl_ok(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&path));
    // A second open while a descriptor is attached is a bad argument, not a leak.
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&path)),
        Err(BeamError::BadArg)
    ));
    ctl_ok(&mut registry, &mut poller, id, "close", b"");

    registry.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// position / seek / sync / truncate
// ---------------------------------------------------------------------------

/// `position`/`seek`/`truncate`/`sync` mirror the oracle: the offset moves as `lseek(2)`
/// does, the reply is the resulting absolute position, `truncate` with no argument cuts at
/// the **current** position (the C's `truncate/1`) and `sync` flushes.
#[test]
fn position_seek_truncate_and_sync_drive_the_file_offset() {
    let scratch = Scratch::new("position");
    let path = scratch.file("pos.txt");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    let mut arg = b"write ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);

    write_all(&mut registry, &mut poller, id, b"0123456789");
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "position", b""),
        b"10",
        "the offset advanced by the write"
    );
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "position", b"3"),
        b"3",
        "an absolute seek replies the new offset"
    );
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "seek", b"eof,0"),
        b"10",
        "`eof` seeks to the end"
    );
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "seek", b"cur,-1"),
        b"9",
        "a relative seek accepts a negative offset"
    );
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "seek", b"bof"),
        b"0",
        "the offset defaults to 0"
    );

    // `truncate` with no argument cuts at the current position: seek to 3, then truncate.
    assert_eq!(ctl_ok(&mut registry, &mut poller, id, "position", b"3"), b"3");
    ctl_ok(&mut registry, &mut poller, id, "truncate", b"");
    ctl_ok(&mut registry, &mut poller, id, "sync", b"");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(content(&path), b"012".to_vec());

    registry.close(&mut poller, id).expect("closes the port");
}

/// `truncate <N>` cuts at an explicit absolute length, independent of the offset.
#[test]
fn truncate_accepts_an_explicit_length() {
    let scratch = Scratch::new("truncate_len");
    let path = scratch.file("trunc.txt");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    let mut arg = b"write ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"0123456789");

    ctl_ok(&mut registry, &mut poller, id, "truncate", b"4");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    assert_eq!(content(&path), b"0123".to_vec());

    registry.close(&mut poller, id).expect("closes the port");
}

/// The position/seek/truncate/sync verbs need an attached descriptor and a well-formed
/// argument: an unopened (or closed) port and a malformed `seek`/`position` argument are the
/// typed `BadArg`, never a wrong result.
#[test]
fn position_verbs_without_a_file_or_argument_are_bad_arg() {
    let scratch = Scratch::new("position_bad");
    let path = scratch.file("f.txt");
    std::fs::write(&path, b"x").expect("the fixture is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    for verb in ["position", "sync", "truncate"] {
        assert!(
            matches!(
                ctl(&mut registry, &mut poller, id, verb, b""),
                Err(BeamError::BadArg)
            ),
            "`{verb}` on an unopened port is badarg"
        );
    }
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "seek", b"bof,0"),
        Err(BeamError::BadArg)
    ));

    ctl_ok(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&path));
    // Malformed arguments against an open file are badarg too.
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "position", b"not-a-number"),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "seek", b"sideways,0"),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "seek", b"bof,not-a-number"),
        Err(BeamError::BadArg)
    ));

    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    // After `close` the verbs are badarg again (no descriptor is attached).
    assert!(matches!(
        ctl(&mut registry, &mut poller, id, "position", b""),
        Err(BeamError::BadArg)
    ));

    registry.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// binary / list data mode
// ---------------------------------------------------------------------------

/// `binary` and `list` are the two `file:` data-representation options: each selects the raw
/// byte phase (there is no line framing in `file:`) and records the mode the runtime reads
/// back through the accessor.
#[test]
fn binary_and_list_select_the_data_mode_and_the_raw_phase() {
    let scratch = Scratch::new("data_mode");
    let path = scratch.file("mode.txt");
    std::fs::write(&path, b"a\nb\n").expect("the fixture is written");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);
    ctl_ok(&mut registry, &mut poller, id, "open", &Scratch::bytes_of(&path));

    ctl_ok(&mut registry, &mut poller, id, "line", b"4");
    assert_eq!(ctl_ok(&mut registry, &mut poller, id, "getopts", b""), b"line,4");

    ctl_ok(&mut registry, &mut poller, id, "list", b"");
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "getopts", b""),
        b"binary",
        "`list` reads the raw byte stream"
    );
    assert_eq!(
        registry.get(id).expect("live").driver_name(),
        "efile",
        "the driver name still reports the port as an efile port"
    );

    // The `open` flag words set the same mode at open time.
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    let mut arg = b"read,list ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    assert_eq!(
        ctl_ok(&mut registry, &mut poller, id, "getopts", b""),
        b"binary"
    );

    ctl_ok(&mut registry, &mut poller, id, "binary", b"");
    assert_eq!(ctl_ok(&mut registry, &mut poller, id, "getopts", b""), b"binary");

    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    registry.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// The oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// `None` — after printing a clear skip message — when `erl` cannot be spawned or does not
/// answer within [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the
/// embedded expression, so that is asserted rather than skipped.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code)
            .output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?");
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

/// The Erlang side of the cross-check: run the same operation sequence against the real
/// `efile` layer (`file:open/2` + `write/2` + `position/2` + `truncate/1` + `sync/1`) and
/// print the observable outcome as `|`-separated fields.
///
/// The fields are: the error atom of a read-only open of a directory, the error atom of a
/// read-only open of a missing path, the offset a `{bof,3}` seek lands on, the file bytes
/// after the truncate-at-position, the file bytes after an `O_RDWR` write, the file bytes
/// after an `O_APPEND` write, the error atom of an exclusive open of an existing path, and
/// the error atom of an exclusive creating open under a regular file.
fn oracle_expression(dir: &str) -> String {
    format!(
        "D = \"{dir}\", P = D ++ \"/t.txt\", \
         E1 = case file:open(D, [raw, read]) of {{error, R1}} -> atom_to_list(R1); _ -> \"ok\" end, \
         E2 = case file:open(D ++ \"/nope\", [raw, read]) of {{error, R2}} -> atom_to_list(R2); _ -> \"ok\" end, \
         {{ok, F}} = file:open(P, [raw, write]), \
         ok = file:write(F, \"0123456789\"), \
         {{ok, Pos}} = file:position(F, {{bof, 3}}), \
         ok = file:truncate(F), \
         ok = file:sync(F), \
         ok = file:close(F), \
         {{ok, C1}} = file:read_file(P), \
         {{ok, F2}} = file:open(P, [raw, read, write]), \
         ok = file:write(F2, \"XY\"), \
         ok = file:close(F2), \
         {{ok, C2}} = file:read_file(P), \
         {{ok, F3}} = file:open(P, [raw, append]), \
         ok = file:write(F3, \"ZZ\"), \
         ok = file:close(F3), \
         {{ok, C3}} = file:read_file(P), \
         E3 = case file:open(P, [raw, write, exclusive]) of {{error, R3}} -> atom_to_list(R3); _ -> \"ok\" end, \
         E4 = case file:open(P ++ \"/sub\", [raw, write, exclusive]) of {{error, R4}} -> atom_to_list(R4); _ -> \"ok\" end, \
         io:format(\"~s|~s|~s|~s|~s|~s|~s|~s\", \
                   [E1, E2, integer_to_list(Pos), C1, C2, C3, E3, E4])"
    )
}

/// The compatibility gate for the file operations this driver adds: the identical sequence
/// is run against the real `efile` layer in the system Erlang and the observable outcome is
/// compared field for field. If `erl` is absent the check skips loudly.
#[test]
fn the_file_verbs_and_flags_agree_with_the_erl_oracle() {
    let scratch = Scratch::new("oracle");
    let dir = scratch.path().to_string_lossy().into_owned();
    let path = scratch.file("t.txt");

    let Some(oracle) = erl_stdout(&oracle_expression(&dir)) else {
        return;
    };

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = open_efile(&mut registry, &mut poller);

    // 1. a read-only open of a directory ⇒ `eisdir`.
    let e1 = match ctl(&mut registry, &mut poller, id, "open", scratch.bytes()) {
        Err(err) => errno_atom(&err).unwrap_or("ok").to_string(),
        Ok(_) => "ok".to_string(),
    };
    // 2. a read-only open of a missing path ⇒ `enoent`.
    let missing = scratch.file("nope");
    let e2 = match ctl(
        &mut registry,
        &mut poller,
        id,
        "open",
        &Scratch::bytes_of(&missing),
    ) {
        Err(err) => errno_atom(&err).unwrap_or("ok").to_string(),
        Ok(_) => "ok".to_string(),
    };

    // 3./4. write, seek to 3, truncate at the position, sync.
    let mut arg = b"write ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"0123456789");
    let pos = String::from_utf8(ctl_ok(&mut registry, &mut poller, id, "position", b"3"))
        .expect("the position reply is ASCII");
    ctl_ok(&mut registry, &mut poller, id, "truncate", b"");
    ctl_ok(&mut registry, &mut poller, id, "sync", b"");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    let c1 = String::from_utf8(content(&path)).expect("ASCII fixture bytes");

    // 5. a read+write open must not truncate.
    let mut arg = b"read,write ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"XY");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    let c2 = String::from_utf8(content(&path)).expect("ASCII fixture bytes");

    // 6. an append open writes at end of file.
    let mut arg = b"append ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    ctl_ok(&mut registry, &mut poller, id, "open", &arg);
    write_all(&mut registry, &mut poller, id, b"ZZ");
    ctl_ok(&mut registry, &mut poller, id, "close", b"");
    let c3 = String::from_utf8(content(&path)).expect("ASCII fixture bytes");

    // 7. an exclusive open of an existing path ⇒ `eexist`.
    let mut arg = b"write,exclusive ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&path));
    let e3 = match ctl(&mut registry, &mut poller, id, "open", &arg) {
        Err(err) => errno_atom(&err).unwrap_or("ok").to_string(),
        Ok(_) => "ok".to_string(),
    };
    // 8. an exclusive creating open under a regular file ⇒ `enotdir`.
    let under = path.join("sub");
    let mut arg = b"write,exclusive ".to_vec();
    arg.extend_from_slice(&Scratch::bytes_of(&under));
    let e4 = match ctl(&mut registry, &mut poller, id, "open", &arg) {
        Err(err) => errno_atom(&err).unwrap_or("ok").to_string(),
        Ok(_) => "ok".to_string(),
    };

    let rust = format!("{e1}|{e2}|{pos}|{c1}|{c2}|{c3}|{e3}|{e4}");
    assert_eq!(
        rust,
        oracle.trim(),
        "the efile driver's file operations must match the system Erlang's"
    );

    registry.close(&mut poller, id).expect("closes the port");
}
