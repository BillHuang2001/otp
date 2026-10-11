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

//! The module checksum (`Module::checksum`) against the **oracle**.
//!
//! [`beam_loader::load_file`] computes the 16-byte MD5 the C loader stores in
//! `beam->checksum` (`erts/emulator/beam/beam_file.c:1751-1830`) and exposes it as
//! [`beam_loader::Module::checksum`]. The reference/ground truth is the standard
//! library's [`beam_lib:md5/1`], which hashes the same fixed chunk subset
//! (`beam_lib.erl:1380-1382`, `filter_funtab/1` at `beam_lib.erl:873-883`).
//!
//! # What this proves
//!
//! * **Committed fixtures** — `slice.beam`, `coverage.beam`, `records_fixture.beam`,
//!   `literals_verbatim.beam`, `literals_compressed.beam`, `bsalloc.beam` — each
//!   digest to a **pinned** hex constant, so the assertion is meaningful even with no
//!   `erl` on `PATH`. The pinned constants were derived from `beam_lib:md5/1` on the
//!   system OTP 29 (`erts-17.1`) oracle.
//! * **Live oracle** — for the fixtures *and* a generically-discovered sample of real
//!   installed OTP modules, this drives the system `erl` and asserts byte-for-byte
//!   agreement with `beam_lib:md5/1`, so the pinned constants cannot silently rot.
//! * **Cross-path consistency** — the digest on a loaded `Module` equals
//!   [`beam_loader::checksum::module_md5`] over that module's raw chunk directory, so
//!   the two public paths cannot diverge.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! A missing OTP install (no `erl` on `PATH`, or the `lib` root cannot be read) makes
//! the *live-oracle* part print `SKIP: <reason>` and fall back to the pinned
//! constants — it never fails and never silently passes without OTP. When the oracle
//! IS available, a mismatch is a hard failure.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader --test checksum
//! ```
//!
//! [`beam_lib:md5/1`]: https://www.erlang.org/doc/apps/stdlib/beam_lib.html#md5-1

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::checksum::module_md5;
use beam_loader::{load, load_file, Module};

mod common;
use common::{all_otp_beams, rel_label, skip_reason};

/// The committed fixtures, paired with the digest the system `beam_lib:md5/1` reports
/// (OTP 29 / `erts-17.1`). Kept lowercase; comparison is case-insensitive.
const FIXTURE_DIGESTS: [(&str, &str); 6] = [
    ("slice.beam", "0285aa0dadcc1f82a1018cd3d10d7424"),
    ("coverage.beam", "06bd5e6eb3bf3f569fcb14bd205ff880"),
    ("records_fixture.beam", "4a53bbc33675ce53a99fd0c56669e540"),
    ("literals_verbatim.beam", "321a85b454a42876496d2212a3f98d0f"),
    ("literals_compressed.beam", "91742cb1ffb37be4ddcb02f7b3a6c5bd"),
    ("bsalloc.beam", "1d46edaa1c5019c6c030987d4931c877"),
];

/// The directory holding the committed `.beam` fixtures.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Renders a 16-byte digest as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Runs `beam_lib:md5/1` over `paths` through the system `erl`, returning a
/// `path → uppercase-hex digest` map, or `None` when `erl` is unavailable or the
/// probe fails.
///
/// The file paths travel as `-extra` plain arguments (`init:get_plain_arguments/0`)
/// so no quoting/escaping of a path into the `-eval` string is needed.
fn oracle_checksums(paths: &[PathBuf]) -> Option<HashMap<String, String>> {
    if paths.is_empty() {
        return Some(HashMap::new());
    }
    let mut cmd = Command::new("erl");
    cmd.args([
        "-noshell",
        "-eval",
        "lists:foreach(\
             fun(F) -> \
                 case beam_lib:md5(F) of \
                     {ok,{_,M}} -> io:format(\"~s ~s~n\",[binary:encode_hex(M),F]); \
                     Other -> io:format(\"ERR ~s ~tp~n\",[F,Other]) \
                 end \
             end, \
             init:get_plain_arguments()), \
         halt().",
        "-extra",
    ]);
    for path in paths {
        cmd.arg(path);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;

    let mut map = HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ERR ") {
            panic!("beam_lib:md5/1 failed for a module: {rest}");
        }
        let (digest, path) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("unparsable oracle line: {line:?}"));
        map.insert(path.to_string(), digest.to_lowercase());
    }
    Some(map)
}

/// Loads `path` and returns its checksum as lowercase hex.
fn loaded_hex(path: &Path) -> String {
    let module = load_file(path)
        .unwrap_or_else(|err| panic!("failed to load {}: {err:?}", path.display()));
    hex(&module.checksum())
}

// ---------------------------------------------------------------------------
// Committed fixtures — pinned constants (always) + live oracle (when available)
// ---------------------------------------------------------------------------

#[test]
fn committed_fixtures_match_the_pinned_oracle_digests() {
    for (name, expected) in FIXTURE_DIGESTS {
        let path = fixtures_dir().join(name);
        let got = loaded_hex(&path);
        assert_eq!(got, expected, "pinned digest mismatch for {name}");
    }
}

#[test]
fn committed_fixtures_match_the_live_oracle() {
    let paths: Vec<PathBuf> = FIXTURE_DIGESTS
        .iter()
        .map(|(name, _)| fixtures_dir().join(name))
        .collect();

    let Some(oracle) = oracle_checksums(&paths) else {
        eprintln!(
            "SKIP: the live beam_lib:md5/1 oracle could not be driven (no `erl` on PATH) — \
             committed fixtures still assert their pinned digests"
        );
        return;
    };

    for path in &paths {
        let key = path.to_string_lossy().to_string();
        let expected = oracle
            .get(&key)
            .unwrap_or_else(|| panic!("the oracle reported no digest for {key}"));
        let got = loaded_hex(path);
        assert_eq!(
            &got,
            expected,
            "beam_loader digest disagrees with beam_lib:md5/1 for {key}"
        );
    }
}

// ---------------------------------------------------------------------------
// A generically-discovered sample of real installed OTP modules
// ---------------------------------------------------------------------------

/// How many installed modules to sample (the sweep's sorted order makes this
/// deterministic). Enough to be non-vacuous without a per-run `erl` storm.
const SAMPLE_LIMIT: usize = 40;

#[test]
fn installed_otp_modules_match_the_live_oracle() {
    if let Some(reason) = skip_reason("the checksum oracle sweep") {
        eprintln!("{reason}");
        return;
    }
    let Some(sweep) = all_otp_beams() else {
        eprintln!("SKIP: the installed OTP `lib` root could not be discovered/read");
        return;
    };
    assert!(
        sweep.beams.len() >= 100,
        "the discovered OTP install is implausibly small ({} modules) — discovery is broken",
        sweep.beams.len()
    );

    let sample: Vec<PathBuf> = sweep.beams.iter().take(SAMPLE_LIMIT).cloned().collect();

    // Every sampled module must load through the public pipeline.
    let mut got: Vec<(String, String)> = Vec::with_capacity(sample.len());
    for path in &sample {
        assert!(
            load_file(path).is_ok(),
            "sampled module failed to load: {}",
            path.display()
        );
        got.push((path.to_string_lossy().to_string(), loaded_hex(path)));
    }

    // Compare against the live oracle. A missing `erl` skips the *comparison* loudly
    // (the load assertion above already ran); it never silently passes.
    let Some(oracle) = oracle_checksums(&sample) else {
        eprintln!(
            "SKIP: the live beam_lib:md5/1 oracle could not be driven (no `erl` on PATH) — \
             {} sampled modules loaded, but their digests were not cross-checked",
            sample.len()
        );
        return;
    };

    let mut compared = 0usize;
    for (key, actual) in &got {
        let expected = oracle
            .get(key)
            .unwrap_or_else(|| panic!("the oracle reported no digest for {key}"));
        assert_eq!(
            actual, expected,
            "beam_loader digest disagrees with beam_lib:md5/1 for {}",
            rel_label(&sweep.root, Path::new(key))
        );
        compared += 1;
    }
    eprintln!(
        "ORACLE: {compared} installed OTP modules agreed with beam_lib:md5/1 (of {} discovered)",
        sweep.beams.len()
    );
    assert!(compared >= SAMPLE_LIMIT, "the comparison did not run");
}

// ---------------------------------------------------------------------------
// Cross-path / representational checks
// ---------------------------------------------------------------------------

#[test]
fn the_loaded_checksum_equals_the_raw_chunk_digest() {
    // The digest on a loaded `Module` must equal `module_md5` over the *same* file's
    // raw chunk directory — the two public paths cannot diverge.
    for (name, expected) in FIXTURE_DIGESTS {
        let bytes = std::fs::read(fixtures_dir().join(name)).expect("fixture readable");
        let container = beam_loader::container::parse(&bytes).expect("fixture parses");
        assert_eq!(
            hex(&module_md5(&container.chunks).expect("digest")),
            expected,
            "raw-digest path mismatch for {name}"
        );
        assert_eq!(
            hex(&module_md5(&container.chunks).expect("digest")),
            loaded_hex(&fixtures_dir().join(name)),
            "raw path vs `load_file` mismatch for {name}"
        );
    }
}

#[test]
fn a_default_module_has_a_zero_checksum_and_a_loaded_one_does_not() {
    assert_eq!(Module::default().checksum(), [0u8; 16]);
    let module = load(include_bytes!("fixtures/slice.beam")).unwrap();
    assert_ne!(module.checksum(), [0u8; 16]);
    assert_eq!(hex(&module.checksum()), FIXTURE_DIGESTS[0].1);
}

#[test]
fn the_checksum_is_available_through_the_public_re_exports() {
    // `checksum::module_md5` is reachable both by its module path and (for callers that
    // only need the prelude) not at all — the prelude is deliberately left unchanged.
    // This pins the public module path the design chose.
    let bytes = include_bytes!("fixtures/slice.beam");
    let container = beam_loader::container::parse(bytes).unwrap();
    let digest = beam_loader::checksum::module_md5(&container.chunks).unwrap();
    assert_eq!(hex(&digest), FIXTURE_DIGESTS[0].1);
}

#[test]
fn a_synthesized_image_with_a_corrupt_funtab_is_rejected_by_the_public_api() {
    // A hand-built container whose `FunT` payload is not `4 + 24n` bytes: `module_md5`
    // (the public path) must reject it with the C's corrupt-lambda-table error rather
    // than hashing a truncated table.
    use beam_loader::container::{parse, ChunkId, Container, RawChunk};

    // `AtU8` count 0, a `Code` chunk with a valid 16-byte header, and a 5-byte `FunT`.
    let atu8 = [0u8, 0, 0, 0];
    let code = {
        let mut c = Vec::new();
        c.extend_from_slice(&16u32.to_be_bytes()); // head_size
        c.extend_from_slice(&0u32.to_be_bytes()); // format_number
        c.extend_from_slice(&181u32.to_be_bytes()); // max_opcode
        c.extend_from_slice(&0u32.to_be_bytes()); // label_count
        c.extend_from_slice(&0u32.to_be_bytes()); // function_count
        c
    };
    let funt = [0u8, 0, 0, 0, 0xFF]; // 5 bytes: 4-byte count + a stray byte

    let mut body = Vec::new();
    for (id, data) in [
        (ChunkId::ATOM, atu8.as_slice()),
        (ChunkId::CODE, code.as_slice()),
        (ChunkId::FUNT, funt.as_slice()),
    ] {
        body.extend_from_slice(&id.name());
        body.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        let pad = (4 - (data.len() % 4)) % 4;
        body.extend(std::iter::repeat_n(0u8, pad));
    }
    let mut image = Vec::new();
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    image.extend_from_slice(b"BEAM");
    image.extend_from_slice(&body);

    let container = parse(&image).expect("the container itself parses");
    assert_eq!(
        container.chunks.iter().filter(|c| c.id == ChunkId::FUNT).count(),
        1
    );
    let err = module_md5(&container.chunks).unwrap_err();
    assert!(
        matches!(err, beam_common::BeamError::BadArg),
        "got {err:?}"
    );

    // A directly-constructed `Container` (the other public shape) behaves the same.
    let direct = Container {
        chunks: vec![
            RawChunk { id: ChunkId::ATOM, data: &atu8 },
            RawChunk { id: ChunkId::CODE, data: &code },
            RawChunk { id: ChunkId::FUNT, data: &funt },
        ],
    };
    assert!(matches!(
        module_md5(&direct.chunks).unwrap_err(),
        beam_common::BeamError::BadArg
    ));
}
