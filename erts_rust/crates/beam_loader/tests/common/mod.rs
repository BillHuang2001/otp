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

// Several helpers here are used by only some of the test files that declare
// `mod common;` (each test file compiles this module independently), so silence
// the per-compilation-unit dead-code lint at the module level.
#![allow(dead_code)]

//! Shared support for the `beam_loader` integration tests.
//!
//! Two responsibilities:
//!
//! 1. **Locating and sweeping the installed OTP install** — the generic,
//!    `code:lib_dir()`-based discovery (no hardcoded application names) shared by
//!    `tests/real_modules.rs` (the broad LOAD sweep) and `tests/generic_census.rs`
//!    (the residual `Instr::Generic` census). [`all_otp_beams`] resolves the `lib`
//!    root and returns every `.beam` beneath it; [`skip_reason`] is the shared
//!    "skip loudly when OTP is absent" predicate both files use.
//!
//! 2. **Raw (non-normalizing) `LitT` payload inspection.** [`beam_codec::decode_term`]
//!    normalizes a flat map's keys into ERTS's `erts_cmp_flatmap_keys` order, so the
//!    key order the compiler wrote (`+deterministic`) is only observable in the
//!    payload bytes themselves. [`raw_map_key_names`] walks a payload with
//!    [`beam_codec::Reader`] — the bounds-checked big-endian cursor, and deliberately
//!    *not* the decoder — and never materializes a term. Used by `tests/literals.rs`.
//!
//! This module is a `tests/` subdirectory module, so Cargo does not build it as a
//! test target of its own; it is compiled into whichever test file declares
//! `mod common;`.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_codec::tag as etf;
use beam_codec::Reader;

// ---------------------------------------------------------------------------
// Locating the installed OTP `lib` root and its `ebin` directories
// ---------------------------------------------------------------------------

/// The running Erlang's `lib` root — the directory holding the versioned
/// application directories (`stdlib-2.0`, `kernel-11.0`, …) — or `None` when it
/// cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_LIB_DIR` when set (it must be the `lib` root);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir()]),halt().'` evaluated at
///    test time (OTP's `code:lib_dir/0`).
///
/// A caller that gets `None` must **skip loudly** (never fail) — see
/// [`skip_reason`].
pub fn otp_lib_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ERTS_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }

    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// The `ebin` directory of the running Erlang's `app` application (`stdlib`,
/// `kernel`, `compiler`, …), or `None` when it cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_<APP>_DIR` (uppercased app name; accepts either the application dir
///    — `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(<app>)]),halt().'`
///    evaluated at test time (OTP's `code:lib_dir/1` returns the *application*
///    dir, whose `ebin` subdirectory holds the `.beam` files).
///
/// A caller that gets `None` must **skip loudly** (never fail) — see
/// [`skip_reason`].
pub fn app_ebin(app: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(format!("ERTS_{}_DIR", app.to_uppercase())) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        return has_beams(&ebin).then_some(ebin);
    }

    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let app_dir = PathBuf::from(stdout.trim());
    if has_beams(&app_dir) {
        return Some(app_dir);
    }
    let ebin = app_dir.join("ebin");
    has_beams(&ebin).then_some(ebin)
}

/// True when `dir` is a directory that contains at least one `*.beam` file.
pub fn has_beams(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .map(|extension| extension == "beam")
            .unwrap_or(false)
    })
}

/// The reason the test should skip (and return early), or `None` when the oracle
/// OTP install is available. `stdlib` and `kernel` are required; a caller that
/// gets `Some` must print it and return — never fail.
///
/// `context` names the calling sweep (e.g. `"the real-module sweep"`) so the
/// printed `SKIP:` line says which evidence could not run.
pub fn skip_reason(context: &str) -> Option<String> {
    if app_ebin("stdlib").is_none() {
        return Some(format!(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             {context} requires the installed OTP `.beam` files"
        ));
    }
    if app_ebin("kernel").is_none() {
        return Some(format!(
            "SKIP: system Erlang/kernel not found (need `erl` on PATH or $ERTS_KERNEL_DIR) — \
             {context} requires the installed OTP `.beam` files"
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Discovering every `.beam` under the OTP `lib` root
// ---------------------------------------------------------------------------

/// The discovered OTP install: the versioned application directories that hold at
/// least one `.beam`, and every `.beam` file found beneath the `lib` root.
pub struct OtpSweep {
    pub root: PathBuf,
    /// Number of versioned application directories that contributed at least one
    /// `.beam` file.
    pub app_count: usize,
    /// Every `.beam` file under the root, in sorted order.
    pub beams: Vec<PathBuf>,
}

/// Recursively collects every `*.beam` file beneath `dir` into `out`.
fn collect_beams(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_beams(&path, out);
        } else if path.extension().map(|ext| ext == "beam").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Discovers the installed OTP applications and every `.beam` under `root`. Each
/// direct subdirectory of `root` is a versioned application directory; one that
/// yields no `.beam` (e.g. `erl_interface`, which ships no `ebin`) is not counted.
///
/// Returns `None` only when `root` is not a readable directory — a caller that
/// gets `None` must **skip loudly** (never fail).
pub fn lib_sweep(root: &Path) -> Option<OtpSweep> {
    let mut apps: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    apps.sort();

    let mut app_count = 0usize;
    let mut beams = Vec::new();
    for app in &apps {
        let mut app_beams = Vec::new();
        collect_beams(app, &mut app_beams);
        if !app_beams.is_empty() {
            app_count += 1;
            beams.append(&mut app_beams);
        }
    }
    beams.sort();
    Some(OtpSweep {
        root: root.to_path_buf(),
        app_count,
        beams,
    })
}

/// The whole installed OTP install, discovered generically: resolves the `lib` root
/// with [`otp_lib_dir`] and sweeps it with [`lib_sweep`]. `None` when the root
/// cannot be resolved or read — a caller that gets `None` must **skip loudly**
/// (never fail).
pub fn all_otp_beams() -> Option<OtpSweep> {
    let root = otp_lib_dir()?;
    lib_sweep(&root)
}

/// A short, stable label for reporting: `path` relative to `root` when possible.
pub fn rel_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

// ---------------------------------------------------------------------------
// Raw (non-normalizing) `LitT` payload inspection
// ---------------------------------------------------------------------------

/// The **raw** (payload) key-name order of the flat map at element 0 of the
/// `nested/0` literal fixture term.
///
/// Reads the payload's own bytes (never `decode_term`, which would normalize the key
/// order away): `[VERSION_MAGIC][SMALL_TUPLE_EXT, 3][MAP_EXT, N][(atom key, value)*]`.
pub fn raw_map_key_names(payload: &[u8]) -> Vec<String> {
    let what = "the `nested/0` literal payload";
    let mut reader = Reader::new(payload);
    assert_eq!(read_u8(&mut reader, what), etf::VERSION_MAGIC, "{what}: the leading version byte");
    assert_eq!(
        read_u8(&mut reader, what),
        etf::SMALL_TUPLE_EXT,
        "{what}: the outer term is a small tuple"
    );
    assert_eq!(read_u8(&mut reader, what), 3, "{what}: the outer tuple has three elements");
    assert_eq!(read_u8(&mut reader, what), etf::MAP_EXT, "{what}: element 0 is a flat map");
    let pairs = read_u32(&mut reader, what);
    assert_eq!(pairs, 3, "{what}: the inner map has three pairs");

    let mut names = Vec::with_capacity(pairs as usize);
    for _ in 0..pairs {
        names.push(read_raw_atom(&mut reader, what));
        skip_raw_value(&mut reader, what);
    }
    names
}

/// Reads one byte through `reader`, or fails naming `what`.
fn read_u8(reader: &mut Reader<'_>, what: &str) -> u8 {
    reader.u8().unwrap_or_else(|error| panic!("{what}: {error:?}"))
}

/// Reads a big-endian `u16` through `reader`, or fails naming `what`.
fn read_u16(reader: &mut Reader<'_>, what: &str) -> u16 {
    reader.u16be().unwrap_or_else(|error| panic!("{what}: {error:?}"))
}

/// Reads a big-endian `u32` through `reader`, or fails naming `what`.
fn read_u32(reader: &mut Reader<'_>, what: &str) -> u32 {
    reader.u32be().unwrap_or_else(|error| panic!("{what}: {error:?}"))
}

/// Borrows `n` bytes through `reader`, or fails naming `what`.
fn read_bytes(reader: &mut Reader<'_>, n: usize, what: &str) -> Vec<u8> {
    reader.bytes(n).unwrap_or_else(|error| panic!("{what}: {error:?}")).to_vec()
}

/// Skips `n` bytes through `reader`, or fails naming `what`.
fn skip_bytes(reader: &mut Reader<'_>, n: usize, what: &str) {
    reader.bytes(n).unwrap_or_else(|error| panic!("{what}: {error:?}"));
}

/// Reads the body of one raw ETF atom **whose tag byte has already been consumed**,
/// and returns its name.
fn read_atom_body(reader: &mut Reader<'_>, tag: u8, what: &str) -> String {
    let length = match tag {
        etf::SMALL_ATOM_UTF8_EXT | etf::SMALL_ATOM_EXT => usize::from(read_u8(reader, what)),
        etf::ATOM_UTF8_EXT | etf::ATOM_EXT => usize::from(read_u16(reader, what)),
        other => panic!("{what}: expected an atom tag, found {other}"),
    };
    let bytes = read_bytes(reader, length, what);
    String::from_utf8(bytes).unwrap_or_else(|error| panic!("{what}: {error}"))
}

/// Reads one raw ETF atom — any of the four wire forms — and returns its name.
fn read_raw_atom(reader: &mut Reader<'_>, what: &str) -> String {
    let tag = read_u8(reader, what);
    read_atom_body(reader, tag, what)
}

/// Advances `reader` past one raw ETF value **without decoding it**.
///
/// NARROW BY DESIGN: it understands the tags the committed fixtures' payloads can
/// carry and panics — naming the offending tag and offset — on any other, so a
/// fixture that grows a new tag fails loudly instead of silently mis-scanning the
/// map's key order.
fn skip_raw_value(reader: &mut Reader<'_>, what: &str) {
    let offset = reader.position();
    let tag = read_u8(reader, what);
    match tag {
        etf::SMALL_INTEGER_EXT => skip_bytes(reader, 1, what),
        etf::INTEGER_EXT => skip_bytes(reader, 4, what),
        etf::NEW_FLOAT_EXT => skip_bytes(reader, 8, what),
        etf::FLOAT_EXT => skip_bytes(reader, 31, what),
        etf::NIL_EXT => {}
        etf::SMALL_ATOM_UTF8_EXT
        | etf::SMALL_ATOM_EXT
        | etf::ATOM_UTF8_EXT
        | etf::ATOM_EXT => {
            // The tag is already consumed, so skip the atom's body directly.
            read_atom_body(reader, tag, what);
        }
        etf::STRING_EXT => {
            let n = usize::from(read_u16(reader, what));
            skip_bytes(reader, n, what);
        }
        etf::BINARY_EXT => {
            let n = read_u32(reader, what) as usize;
            skip_bytes(reader, n, what);
        }
        etf::BIT_BINARY_EXT => {
            let n = read_u32(reader, what) as usize;
            skip_bytes(reader, n + 1, what);
        }
        etf::SMALL_BIG_EXT => {
            let n = usize::from(read_u8(reader, what));
            skip_bytes(reader, n + 1, what);
        }
        etf::LARGE_BIG_EXT => {
            let n = read_u32(reader, what) as usize;
            skip_bytes(reader, n + 1, what);
        }
        etf::EXPORT_EXT => {
            read_raw_atom(reader, what);
            read_raw_atom(reader, what);
        }
        etf::LIST_EXT => {
            let n = read_u32(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
            }
            // ... plus the tail, which `NIL_EXT` terminates for a proper list.
            skip_raw_value(reader, what);
        }
        etf::SMALL_TUPLE_EXT => {
            let n = read_u8(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
            }
        }
        etf::LARGE_TUPLE_EXT => {
            let n = read_u32(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
            }
        }
        etf::MAP_EXT => {
            let n = read_u32(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
                skip_raw_value(reader, what);
            }
        }
        other => panic!(
            "{what}: unmodelled ETF tag {other} at offset {offset} — extend `skip_raw_value`"
        ),
    }
}
