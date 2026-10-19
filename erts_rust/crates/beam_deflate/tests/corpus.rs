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
//! The manual **corpus sweep**: run the compressor over a directory of real inputs, then
//! compare against the oracle's own images for the same inputs.
//!
//! This is the file-set sweep the fixed test matrices cannot be — it takes whatever binary
//! corpus is on disk (a `*.bin` file per input), writes this port's three framings for each
//! of them, and compares them byte for byte against the images zlib 1.3.2 produced. It
//! drives [`Deflate::compress_with`] directly, so it measures the shipped one-shot path
//! (the `zlib:compress/1` / `zlib:zip/1` / `zlib:gzip/1` schedule) rather than a
//! test-local shortcut.
//!
//! Both legs are `#[ignore]`d and driven by environment variables, so a plain `cargo test`
//! never touches the file system:
//!
//! ```text
//! # 1. this port's images: <stem>.cand (zlib), <stem>.rzip (raw), <stem>.rgzip (gzip)
//! BD_CORPUS=/path/to/*.bin BD_OUT=/tmp/bd-out \
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_deflate --test corpus -- --ignored --nocapture corpus_sweep
//!
//! # 2. the oracle's images for the same inputs, in ONE erl boot, at the same level
//! mkdir -p /tmp/bd-oracle && \
//! BD_CORPUS=/path/to/*.bin BD_ORACLE_OUT=/tmp/bd-oracle erl -noshell -eval '
//!   Dir=os:getenv("BD_CORPUS"), Out=os:getenv("BD_ORACLE_OUT"),
//!   {ok,Names}=file:list_dir(Dir),
//!   [begin
//!      {ok,B}=file:read_file(filename:join(Dir,N)),
//!      Stem=filename:basename(N,".bin"),
//!      file:write_file(filename:join(Out,Stem++".cand"),zlib:compress(B)),
//!      file:write_file(filename:join(Out,Stem++".rzip"),zlib:zip(B)),
//!      file:write_file(filename:join(Out,Stem++".rgzip"),zlib:gzip(B))
//!    end || N <- lists:sort(Names), filename:extension(N) =:= ".bin"],
//!   halt().'
//!
//! # 3. byte-identity: this port vs the oracle, over every input
//! BD_ORACLE_IN=/path/to/*.bin BD_ORACLE_OUT=/tmp/bd-oracle \
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_deflate --test corpus -- --ignored --nocapture corpus_matches_the_oracle
//! ```
//!
//! `BD_LEVELS` (a comma-separated list such as `0,1,6,9`) sweeps several levels instead of
//! the single default one; with more than one level the images are named
//! `<stem>.l<LEVEL>.<ext>` so they cannot collide. The oracle recipe above uses
//! `zlib:compress/1` & co., which are `Z_DEFAULT_COMPRESSION` (level 6), so it matches
//! `BD_LEVELS` unset or `-1` (and `6`, which is the same stream).


use std::ffi::OsString;
use std::path::{Path, PathBuf};

use beam_deflate::{Deflate, DeflateConfig, DEFAULT_COMPRESSION};

/// The three framings a sweep writes, with the extension each one gets.
///
/// The names are the `zlib` module's function names' initials — `compress/1` (zlib),
/// `zip/1` (raw), `gzip/1` (gzip) — which is also how the `beam_bifs` sweep names them, so
/// one shell loop can compare the two ports' outputs.
const FRAMINGS: [(&str, &str); 3] = [("cand", "zlib"), ("rzip", "raw"), ("rgzip", "gzip")];

/// The configured levels: `$BD_LEVELS` (`0,1,6,9`) or just the default level.
fn levels() -> Vec<i8> {
    match std::env::var("BD_LEVELS") {
        Ok(spec) => {
            let levels: Vec<i8> = spec
                .split(',')
                .map(|part| {
                    part.trim()
                        .parse::<i8>()
                        .unwrap_or_else(|_| panic!("BD_LEVELS: {part:?} is not a level"))
                })
                .collect();
            assert!(!levels.is_empty(), "BD_LEVELS is empty");
            levels
        }
        Err(_) => vec![DEFAULT_COMPRESSION],
    }
}

/// The `DeflateConfig` for one framing at `level` (`memLevel` 8, default strategy — what
/// `zlib:compress/1`, `zlib:zip/1` and `zlib:gzip/1` use).
fn config(framing: &str, level: i8) -> DeflateConfig {
    match framing {
        "zlib" => DeflateConfig::zlib(level),
        "raw" => DeflateConfig::raw(level),
        "gzip" => DeflateConfig::gzip(level),
        other => panic!("unknown framing {other:?}"),
    }
}

/// `<dir>/<stem>[.l<level>].<ext>` — the level is only spelled out when more than one is
/// swept, so the single-level names stay the plain `<stem>.cand` form.
fn image_path(dir: &Path, stem: &OsString, level: i8, levels: &[i8], ext: &str) -> PathBuf {
    let mut name = stem.clone();
    if levels.len() > 1 {
        name.push(format!(".l{level}"));
    }
    name.push(format!(".{ext}"));
    dir.join(name)
}

/// Every `*.bin` in `dir`, sorted, so a sweep is reproducible.
fn inputs(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    paths.sort();
    paths
}

/// The encode direction: this port's three framings for every corpus input.
#[test]
#[ignore = "manual corpus sweep; set BD_CORPUS and BD_OUT"]
fn corpus_sweep() {
    let Ok(corpus) = std::env::var("BD_CORPUS") else {
        println!("SKIP corpus_sweep: BD_CORPUS is not set");
        return;
    };
    let out_dir = PathBuf::from(std::env::var("BD_OUT").expect("BD_OUT with BD_CORPUS"));
    std::fs::create_dir_all(&out_dir).expect("create BD_OUT");
    let levels = levels();

    let (mut inputs_seen, mut images, mut bytes) = (0usize, 0usize, 0usize);
    let mut largest = (0usize, String::new());
    for path in inputs(Path::new(&corpus)) {
        let data = std::fs::read(&path).expect("read corpus input");
        let stem = path
            .file_stem()
            .expect("a corpus file has a stem")
            .to_os_string();
        for &level in &levels {
            for (ext, framing) in FRAMINGS {
                let image = Deflate::compress_with(config(framing, level), &data).unwrap_or_else(
                    |e| panic!("{framing} level {level} of {}: {e:?}", path.display()),
                );
                let dest = image_path(&out_dir, &stem, level, &levels, ext);
                std::fs::write(&dest, &image)
                    .unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
                if data.len() > largest.0 {
                    largest = (data.len(), path.display().to_string());
                }
                images += 1;
                bytes += image.len();
            }
        }
        inputs_seen += 1;
    }
    println!(
        "corpus_sweep: {inputs_seen} inputs x {} levels x 3 framings = {images} images \
         ({bytes} bytes) -> {}; largest input {} bytes ({})",
        levels.len(),
        out_dir.display(),
        largest.0,
        largest.1
    );
}

/// The comparison direction: this port's images must be **byte-identical** to the oracle's.
///
/// `BD_ORACLE_IN` supplies the original `*.bin` inputs and `BD_ORACLE_OUT` the oracle's
/// images for them (same names the sweep writes — see the module docs). A divergence is
/// reported with the first differing offset, so it can be diffed against `deflate.c`
/// directly.
#[test]
#[ignore = "manual corpus comparison; set BD_ORACLE_IN and BD_ORACLE_OUT"]
fn corpus_matches_the_oracle() {
    let (Ok(originals), Ok(images)) = (
        std::env::var("BD_ORACLE_IN"),
        std::env::var("BD_ORACLE_OUT"),
    ) else {
        println!("SKIP corpus_matches_the_oracle: BD_ORACLE_IN / BD_ORACLE_OUT are not set");
        return;
    };
    let images = PathBuf::from(images);
    let levels = levels();

    let (mut compared, mut inputs_seen) = (0usize, 0usize);
    let mut failures = Vec::new();
    for path in inputs(Path::new(&originals)) {
        let data = std::fs::read(&path).expect("read corpus input");
        let stem = path
            .file_stem()
            .expect("a corpus file has a stem")
            .to_os_string();
        for &level in &levels {
            for (ext, framing) in FRAMINGS {
                let ours = Deflate::compress_with(config(framing, level), &data).unwrap_or_else(
                    |e| panic!("{framing} level {level} of {}: {e:?}", path.display()),
                );
                let oracle_path = image_path(&images, &stem, level, &levels, ext);
                let want = std::fs::read(&oracle_path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", oracle_path.display()));
                if ours != want {
                    let at = ours
                        .iter()
                        .zip(want.iter())
                        .position(|(a, b)| a != b)
                        .unwrap_or(ours.len().min(want.len()));
                    failures.push(format!(
                        "{framing} level {level} of {} ({} bytes): {} bytes here, {} in {}, \
                         first difference at offset {at}",
                        path.display(),
                        data.len(),
                        ours.len(),
                        want.len(),
                        oracle_path.display()
                    ));
                }
                compared += 1;
            }
        }
        inputs_seen += 1;
    }
    assert!(
        failures.is_empty(),
        "{} of {compared} corpus images disagree with the oracle:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!(
        "corpus_matches_the_oracle: {inputs_seen} inputs x {} levels x 3 framings = {compared} \
         images byte-identical to the oracle",
        levels.len()
    );
}
