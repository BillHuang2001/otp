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

//! Guard: every committed `.beam` fixture under `tests/fixtures/` is free of
//! build-machine absolute paths.
//!
//! A `.beam` records the source file it was compiled from in its `CInf` (compile info —
//! the `source` key) and `Dbgi` (debug info — the `cwd` key), and its `Line` chunk carries
//! a file-name table, so an image produced by a plain `erlc` embeds the directory the build
//! ran in (`/tmp/genesis/…`, `/home/<user>/…`, a CI workspace, a Nix store path). That
//! leaks local layout into the repository and makes the fixture non-reproducible, so the
//! committed fixtures are compiled **`+deterministic`** from a neutral `$TMPDIR` scratch dir
//! (`+deterministic` drops the `source`/`options` keys, the abstract code and the line
//! table's file-name entry; see `tests/fixtures/README.md` for the per-image recipe).
//!
//! # A deliberate crate-local duplicate
//!
//! This file is a **test-only duplicate** of `beam_loader`'s
//! `tests/fixtures_are_path_free.rs`, kept crate-local on purpose: each crate guards the
//! fixtures it owns, and a shared guard would have to live in a fourth crate (a new
//! cross-crate dependency edge bought for a test). The scanner shape is deliberately
//! identical so a reader of either file sees one convention, and the two copies drift only
//! where their fixture trees differ — this one walks the tree **recursively**, so a fixture
//! added in a subdirectory is covered.
//!
//! # What this proves
//!
//! * Every `*.beam` under `tests/fixtures/` (every subdirectory included) is enumerated **at
//!   run time**, so a fixture added later is covered automatically — there is no hard-coded
//!   list to forget.
//! * Every byte of every image is scanned for the markers in [`ABSOLUTE_PATH_MARKERS`] and
//!   for a Windows drive-letter path (`C:\…`); a failure names the fixture, the byte offset
//!   and the bytes around the match. The scan runs over the *raw* bytes, so a marker needs
//!   neither a NUL terminator nor any other framing to be found.
//! * Every chunk payload that *is* a zlib stream is inflated and scanned too: the compiler
//!   stores debug info and documentation as an ETF `COMPRESSED` term and `+compressed_literals`
//!   compresses `LitT`, so a path inside one of those would be invisible to a raw byte scan.
//!
//! The test cannot pass vacuously: it asserts the directory really yielded the committed
//! fixtures, and [`the_scanner_finds_a_synthetic_absolute_path`] pins that the scanner detects
//! `/tmp/x.erl` and `C:\Users\…` while a relative path stays clean;
//! [`compressed_chunk_payloads_are_scanned_too`] pins both compressed framings.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test fixtures_are_path_free
//! ```

use std::path::{Path, PathBuf};

use beam_codec::compress::{decompress_bounded, MAX_UNCOMPRESSED};
use beam_loader::container;

/// The absolute-path roots that must never occur in a committed fixture: they name the
/// machine that built the workspace, never the fixture itself.
///
/// Extend this list when a new build environment leaks through; a marker is matched
/// literally, so `"/tmp/"` also catches `$TMPDIR` paths.
const ABSOLUTE_PATH_MARKERS: &[&str] = &[
    "/home/",
    "/tmp/",
    "/Source/",
    "/Users/",
    "/private/",
    "/nix/store/",
    ".genesis/",
    "/genesis/",
];

/// zlib's CMF byte for a 32 KiB window (`CM = 8`, `CINFO = 7`) — every zlib stream the
/// Erlang toolchain emits starts with it.
const ZLIB_CMF: u8 = 0x78;

/// ETF version magic and `COMPRESSED` tag, the wrapper around a compressed term.
const ETF_VERSION_MAGIC: u8 = 131;
const ETF_COMPRESSED_TAG: u8 = 80;

/// How many bytes of context a [`Finding`] renders on each side of the match.
const CONTEXT_BYTES: usize = 32;

/// One absolute-path occurrence found by [`scan_for_absolute_paths`].
#[derive(Debug, PartialEq, Eq)]
struct Finding {
    /// The marker that matched (`"C:\\"` for the drive-letter form).
    marker: String,
    /// Byte offset of the match in the scanned buffer.
    offset: usize,
    /// The bytes around the match, rendered for a failure message.
    context: String,
}

/// The committed fixture directory.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every `*.beam` under [`fixtures_dir`], **recursively**, sorted by path.
///
/// Enumerated at run time on purpose: a fixture committed later — including one in a new
/// subdirectory — is guarded without touching this file.
fn beam_fixtures() -> Vec<PathBuf> {
    let root = fixtures_dir();
    let mut pending = vec![root.clone()];
    let mut beams: Vec<PathBuf> = Vec::new();

    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|error| {
            panic!("cannot read the fixture dir {}: {error}", dir.display())
        });
        for entry in entries {
            let path = entry.expect("a readable fixture directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "beam") {
                beams.push(path);
            }
        }
    }

    beams.sort();
    beams
}

/// A fixture's name as a failure message shows it: relative to the fixture root, so a
/// nested fixture reads `sub/x.beam` rather than an absolute build path.
fn fixture_name(path: &Path) -> String {
    path.strip_prefix(fixtures_dir())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Every absolute-path occurrence in `bytes`, ordered by offset.
fn scan_for_absolute_paths(bytes: &[u8]) -> Vec<Finding> {
    let mut findings = Vec::new();

    for marker in ABSOLUTE_PATH_MARKERS {
        let needle = marker.as_bytes();
        for (offset, window) in bytes.windows(needle.len()).enumerate() {
            if window == needle {
                findings.push(Finding {
                    marker: (*marker).to_string(),
                    offset,
                    context: render_context(bytes, offset, needle.len()),
                });
            }
        }
    }

    for offset in 0..bytes.len() {
        if let Some(length) = drive_letter_path_len(bytes, offset) {
            findings.push(Finding {
                marker: format!("{}:\\", bytes[offset] as char),
                offset,
                context: render_context(bytes, offset, length),
            });
        }
    }

    findings.sort_by_key(|finding| finding.offset);
    findings
}

/// The length of the Windows drive-letter path starting at `offset` (`C:\…`), or `None`.
///
/// Accepts an ASCII letter, `:`, a separator and then **at least three printable ASCII
/// bytes** — the shape of a real path (`C:\Users\bill\x.erl`) and a shape arbitrary
/// binary data does not produce, so a compressed chunk's noise cannot trip the guard.
fn drive_letter_path_len(bytes: &[u8], offset: usize) -> Option<usize> {
    let letter = *bytes.get(offset)?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    if bytes.get(offset + 1) != Some(&b':') {
        return None;
    }
    if !matches!(bytes.get(offset + 2), Some(b'\\') | Some(b'/')) {
        return None;
    }
    let body = bytes[offset + 3..]
        .iter()
        .take_while(|&&byte| (0x20..=0x7e).contains(&byte))
        .count();
    (body >= 3).then_some(3 + body)
}

/// Renders `bytes[offset .. offset + length]` plus [`CONTEXT_BYTES`] of either side as
/// printable ASCII, with everything else escaped, so a failure message shows the path.
fn render_context(bytes: &[u8], offset: usize, length: usize) -> String {
    use std::fmt::Write as _;

    let start = offset.saturating_sub(CONTEXT_BYTES);
    let end = (offset + length + CONTEXT_BYTES).min(bytes.len());
    let mut out = String::new();
    for &byte in &bytes[start..end] {
        match byte {
            b' '..=b'~' => out.push(byte as char),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            _ => {
                let _ = write!(out, "\\x{byte:02x}");
            }
        }
    }
    out
}

/// Every zlib stream a chunk payload carries, inflated.
///
/// Three framings are recognized: an ETF `COMPRESSED` term
/// (`[131][80][body_len:4][zlib stream]`, which is how the compiler writes `Dbgi` and
/// `Docs`), a bare zlib stream, and `LitT`'s compressed framing
/// (`[uncompressed_len:4][zlib stream]`). A payload that is none of those simply fails
/// to inflate and contributes nothing: inflation is a best-effort *extra* view, while
/// the raw scan of the whole image remains the actual assertion, so a payload that is
/// not a stream is not an error.
fn inflated_views(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut streams: Vec<&[u8]> = Vec::new();

    if payload.len() > 6 && payload[0] == ETF_VERSION_MAGIC && payload[1] == ETF_COMPRESSED_TAG {
        streams.push(&payload[6..]);
    }
    for skip in [0, 4] {
        if let Some(rest) = payload.get(skip..) {
            if rest.first() == Some(&ZLIB_CMF) {
                streams.push(rest);
            }
        }
    }

    streams
        .into_iter()
        .filter_map(|stream| decompress_bounded(stream, MAX_UNCOMPRESSED).ok())
        .collect()
}

#[test]
fn committed_beam_fixtures_contain_no_absolute_paths() {
    let beams = beam_fixtures();
    // Non-vacuity: the tree is enumerated at run time, so assert it really holds the
    // committed images (27 at the time of writing) instead of passing on an empty listing.
    assert!(
        beams.len() >= 20,
        "the fixture tree must yield the committed `.beam` images, found {}: {:?}",
        beams.len(),
        beams
            .iter()
            .map(|path| fixture_name(path))
            .collect::<Vec<_>>()
    );

    let mut failures: Vec<String> = Vec::new();
    let mut scanned_bytes = 0usize;
    for path in &beams {
        let name = fixture_name(path);
        let bytes = std::fs::read(path)
            .unwrap_or_else(|error| panic!("cannot read the fixture {name}: {error}"));
        scanned_bytes += bytes.len();

        // The image must be a well-formed `.beam` first: the guard can only vouch for a
        // fixture whose chunk directory it could walk.
        let image = container::parse(&bytes).unwrap_or_else(|error| {
            panic!("{name} is not a well-formed .beam image, so it cannot be scanned: {error:?}")
        });

        for finding in scan_for_absolute_paths(&bytes) {
            failures.push(format!(
                "{name}: absolute-path marker {:?} at byte {}\n      near: {}",
                finding.marker, finding.offset, finding.context
            ));
        }

        for chunk in &image.chunks {
            let id = String::from_utf8_lossy(&chunk.id.name()).into_owned();
            for view in inflated_views(chunk.data) {
                for finding in scan_for_absolute_paths(&view) {
                    failures.push(format!(
                        "{name} (chunk {id}, inflated): absolute-path marker {:?} at byte {}\n      near: {}",
                        finding.marker, finding.offset, finding.context
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "committed `.beam` fixtures must not embed build-machine absolute paths — regenerate the \
         offending fixture with `erlc +deterministic …` from a neutral scratch dir (the recipe is \
         in `tests/fixtures/README.md`):\n  {}",
        failures.join("\n  ")
    );
    // The enumeration above is the assertion; this only keeps the scan honest about how much
    // it saw, so a fixture list of zero-byte stubs could not report full coverage.
    assert!(
        scanned_bytes > 10_000,
        "the scan must cover the real images, not {} bytes of stubs",
        scanned_bytes
    );
}

#[test]
fn the_scanner_finds_a_synthetic_absolute_path() {
    // A `$TMPDIR`-shaped `source` path in compile info — the exact leakage class this
    // guard exists for.
    let leaked = b"\x83l\x00\x00\x00\x01h\x02w\x06sourcek\x00\x15/tmp/x.erl\x00";
    let at = leaked
        .windows(5)
        .position(|window| window == b"/tmp/")
        .expect("the synthetic image contains the marker");
    let findings = scan_for_absolute_paths(leaked);
    assert_eq!(findings.len(), 1, "one marker expected, got {findings:?}");
    assert_eq!(findings[0].marker, "/tmp/");
    assert_eq!(findings[0].offset, at);
    assert!(
        findings[0].context.contains("/tmp/x.erl"),
        "the failure message must show the leaked path: {:?}",
        findings[0].context
    );

    // The Windows drive-letter form (`C:\Users\bill\x.erl`).
    let windows = b"\x00C:\\Users\\bill\\x.erl\x00";
    let findings = scan_for_absolute_paths(windows);
    assert_eq!(findings.len(), 1, "one drive-letter path expected, got {findings:?}");
    assert_eq!(findings[0].offset, 1);
    assert_eq!(findings[0].marker, "C:\\");

    // Negative controls: relative paths, a bare atom and arbitrary binary stay clean, so
    // the guard cannot fire on an ordinary fixture.
    for clean in [
        b"\x83k\x00\x0bbsalloc.erl".as_slice(),
        b"src/lib/app.erl",
        b"../../erts/emulator/beam/beam_file.c",
        b"\x00\x01\x02\x03",
    ] {
        assert!(
            scan_for_absolute_paths(clean).is_empty(),
            "{:?} must not be reported as an absolute path",
            String::from_utf8_lossy(clean)
        );
    }
}

#[test]
fn compressed_chunk_payloads_are_scanned_too() {
    // `+deterministic` strips the path *before* the compiler compresses the chunk, so a
    // compressed payload is a place a path could otherwise hide. Build one the way the
    // compiler does — an ETF `COMPRESSED` term — and check the inflated view is scanned.
    let payload = b"\x00\x00\x00\x00C:\\Users\\bill\\src\\x.erl";
    let stream = beam_codec::compress::compress(payload, 6).expect("zlib compresses");
    assert!(
        scan_for_absolute_paths(&stream).is_empty(),
        "the compressed bytes themselves carry no readable path"
    );

    let mut wrapped = vec![ETF_VERSION_MAGIC, ETF_COMPRESSED_TAG];
    wrapped.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    wrapped.extend_from_slice(&stream);

    let views = inflated_views(&wrapped);
    assert_eq!(views.len(), 1, "the COMPRESSED wrapper yields one inflated view");
    let findings = scan_for_absolute_paths(&views[0]);
    assert_eq!(
        findings.len(),
        1,
        "the path inside the compressed payload must be found: {findings:?}"
    );
    assert_eq!(findings[0].marker, "C:\\");

    // `LitT`'s framing is a bare zlib stream behind a 4-byte uncompressed length (what
    // `+compressed_literals` writes), so the same path must be reachable through it.
    let mut lit = (payload.len() as u32).to_be_bytes().to_vec();
    lit.extend_from_slice(&stream);
    let views = inflated_views(&lit);
    assert_eq!(views.len(), 1, "the LitT wrapper yields one inflated view");
    let findings = scan_for_absolute_paths(&views[0]);
    assert_eq!(
        findings.len(),
        1,
        "the path inside a compressed literal table must be found: {findings:?}"
    );
    assert_eq!(findings[0].marker, "C:\\");
}
