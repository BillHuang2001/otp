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

//! A **chunk-completeness checker** over the installed OTP corpus.
//!
//! Where `tests/real_modules.rs` proves every installed `.beam` *loads*, this file
//! proves the loader's chunk handling is **complete**: every chunk id any real `.beam`
//! carries is classified in [`beam_loader::chunk::classify`] as
//! *parsed+exposed*, *parsed-as-raw* or *tooling-only*. An id the crate does not
//! recognize — the shape of "an unparsed runtime-relevant chunk slipped in" — fails
//! the test by name.
//!
//! # What the checker proves
//!
//! * **Classification completeness.** It walks the whole installed OTP `lib` root
//!   (every versioned application directory, every `*.beam` recursively), reads each
//!   file's chunk directory with [`beam_loader::container::parse`] and asserts every
//!   id it sees is classified. The *set* of ids and their counts is printed.
//! * **No vacuous pass.** Positive floors require the corpus to contain the ids that
//!   must be present (`AtU8`/`Code`/`Attr`/`CInf`/`Dbgi`/`Docs`/`Meta`/`FunT`/…) — so
//!   the "every id is classified" assertion cannot hold merely because the walk found
//!   nothing.
//! * **Loads still work.** Every discovered module is loaded through the public
//!   pipeline, so a chunk-classification change cannot silently break loading.
//! * **`Attr` decode matches the oracle.** For `gen_server` and `kernel` the decoded
//!   attribute term ([`beam_loader::Module::attributes_term`]) is re-encoded with the
//!   crate's own ETF encoder and compared, byte for byte, with the raw `Attr` chunk
//!   *and* with the system Erlang's own `term_to_binary(Module:module_info(attributes))`.
//! * **`CInf`/`Meta` decode matches the oracle.** The decoded compiler information
//!   ([`beam_loader::Module::compile_info_term`]) and feature metadata
//!   ([`beam_loader::Module::meta_term`]) are re-encoded and compared, byte for byte,
//!   with the raw `CInf`/`Meta` chunk and with the system Erlang's own
//!   `beam_lib:chunks/2` + `term_to_binary` view of the same chunks. The census below
//!   additionally round-trips both chunks for **every** module it loads, with a floor
//!   asserting the check genuinely ran.
//!
//! # Locating the OTP install (generic, no hardcoded application names)
//!
//! Mirrors `tests/real_modules.rs`: the `lib` root is `$ERTS_LIB_DIR` when set,
//! otherwise `erl -noshell -eval 'io:format("~s",[code:lib_dir()])'`.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! A missing OTP install skips LOUDLY (`SKIP: <reason>`) and never fails. It never
//! silently passes without OTP.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader
//! ```
//!
//! With `-- --nocapture` the chunk census and the `CHUNKS: …` summary are printed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::chunk::classify::{self, ChunkClass};
use beam_loader::container::{self, ChunkId};

// ---------------------------------------------------------------------------
// Locating the installed OTP `lib` root and its `ebin` directories
// ---------------------------------------------------------------------------

/// The running Erlang's `lib` root, or `None` when it cannot be located.
///
/// Resolution order (identical to `tests/real_modules.rs`): `$ERTS_LIB_DIR` when set,
/// otherwise OTP's own `code:lib_dir/0`.
fn otp_lib_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ERTS_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }

    let output = Command::new("erl")
        .args(["-noshell", "-eval", "io:format(\"~s\",[code:lib_dir()]),halt()."])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// The `ebin` directory of the running Erlang's `app` application, or `None`.
fn app_ebin(app: &str) -> Option<PathBuf> {
    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let app_dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    if has_beams(&app_dir) {
        return Some(app_dir);
    }
    let ebin = app_dir.join("ebin");
    has_beams(&ebin).then_some(ebin)
}

/// True when `dir` is a directory that contains at least one `*.beam` file.
fn has_beams(dir: &Path) -> bool {
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

/// The reason the test should skip (and return early), or `None` when OTP is available.
fn skip_reason() -> Option<String> {
    if app_ebin("stdlib").is_none() || app_ebin("kernel").is_none() {
        return Some(
            "SKIP: system Erlang (stdlib/kernel) not found (need `erl` on PATH or \
             $ERTS_LIB_DIR) — the chunk census requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    None
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

/// Every versioned application directory under `root` that holds at least one `.beam`,
/// plus every discovered `.beam` (sorted), or `None` when `root` is unreadable.
fn lib_sweep(root: &Path) -> Option<(usize, Vec<PathBuf>)> {
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
    Some((app_count, beams))
}

/// A short, stable label for reporting: `path` relative to `root` when possible.
fn rel_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// A printable rendering of a chunk id (`AtU8`, `Docs`, …).
fn id_name(id: ChunkId) -> String {
    String::from_utf8_lossy(&id.name()).to_string()
}

// ---------------------------------------------------------------------------
// The corpus census
// ---------------------------------------------------------------------------

/// Walks the whole installed OTP `lib` root and asserts every chunk id seen is
/// classified, every module loads, and the census is not vacuous.
#[test]
fn every_corpus_chunk_id_is_classified() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }
    let Some(root) = otp_lib_dir() else {
        eprintln!("SKIP: could not resolve the OTP `lib` root (need `erl` on PATH or $ERTS_LIB_DIR)");
        return;
    };
    let Some((app_count, beams)) = lib_sweep(&root) else {
        eprintln!("SKIP: OTP `lib` root {} is not a readable directory", root.display());
        return;
    };
    assert!(
        !beams.is_empty(),
        "the census found no `.beam` files under {}",
        root.display()
    );

    // Chunk id -> how many modules carry it.
    let mut census: BTreeMap<[u8; 4], usize> = BTreeMap::new();
    // Unknown (unclassified) ids, each with the module that carried it.
    let mut unknown: Vec<(String, String)> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut loaded = 0usize;
    // How many modules actually exercised each decoded term chunk (see the floor below).
    let mut cinf_round_tripped = 0usize;
    let mut meta_round_tripped = 0usize;

    for path in &beams {
        let label = rel_label(&root, path);
        let Ok(bytes) = std::fs::read(path) else {
            failures.push((label, "unreadable file".to_string()));
            continue;
        };

        match container::parse(&bytes) {
            Ok(parsed) => {
                for chunk in &parsed.chunks {
                    *census.entry(chunk.id.name()).or_default() += 1;
                    if !classify::is_known(chunk.id) {
                        unknown.push((label.clone(), id_name(chunk.id)));
                    }
                }
            }
            Err(error) => {
                failures.push((label.clone(), format!("container: {error:?}")));
                continue;
            }
        }

        match beam_loader::load_file(path) {
            Ok(module) => {
                loaded += 1;
                if module.functions.is_empty() {
                    failures.push((label.clone(), "loaded but yielded no function".to_string()));
                }
                // The eagerly decoded term-carrying chunks must round-trip back to the
                // chunk's own bytes: `CInf` and `Meta` are plain `binary_to_term`
                // payloads (`beam_lib.erl:1071-1077` for `CInf`; the C only checksums
                // `Meta`), so re-encoding the decoded term through the crate's own ETF
                // encoder must reproduce the chunk byte for byte — and a chunk that is
                // absent must leave the holder empty.
                for (what, raw, decoded) in [
                    ("CInf", &module.compile_info, module.compile_info_term()),
                    ("Meta", &module.meta, module.meta_term()),
                ] {
                    match (raw.as_deref(), decoded) {
                        (Some(raw), Some(term)) => {
                            match beam_codec::encode_term(term, &module.atom_table) {
                                Ok(reencoded) if reencoded == raw => {
                                    if what == "CInf" {
                                        cinf_round_tripped += 1;
                                    } else {
                                        meta_round_tripped += 1;
                                    }
                                }
                                Ok(_) => failures.push((
                                    label.clone(),
                                    format!("{what} re-encode differs from the chunk"),
                                )),
                                Err(error) => failures
                                    .push((label.clone(), format!("{what} encode: {error:?}"))),
                            }
                        }
                        (None, None) => {}
                        (Some(_), None) => failures.push((
                            label.clone(),
                            format!("{what} chunk present but did not decode"),
                        )),
                        (None, Some(_)) => failures.push((
                            label.clone(),
                            format!("{what} decoded without a chunk"),
                        )),
                    }
                }
            }
            Err(error) => failures.push((label, format!("load: {error:?}"))),
        }
    }

    // Print the classification of every id the corpus carries, with its count.
    let mut report = String::new();
    let _ = writeln!(
        report,
        "CHUNKS: {app_count} application(s), {} module(s) under {}",
        beams.len(),
        root.display()
    );
    for (id, count) in &census {
        let name = id_name(ChunkId(*id));
        match classify::classify(ChunkId(*id)) {
            Some(kind) => {
                let _ = writeln!(
                    report,
                    "  {name:<4} x{count:<5} {:<14} {}",
                    kind.class.label(),
                    kind.reason
                );
            }
            None => {
                let _ = writeln!(report, "  {name:<4} x{count:<5} UNCLASSIFIED");
            }
        }
    }
    let _ = writeln!(
        report,
        "  decoded round-trip: CInf x{cinf_round_tripped}, Meta x{meta_round_tripped}"
    );
    println!("{report}");

    // Positive floors: the corpus genuinely carries the ids that must be classified, so
    // the "every id is classified" assertion below cannot hold vacuously.
    for required in [
        *b"AtU8", *b"Code", *b"Attr", *b"CInf", *b"Dbgi", *b"Docs", *b"Meta", *b"FunT", *b"LitT",
        *b"Line", *b"LocT", *b"Type", *b"StrT", *b"ImpT", *b"ExpT",
    ] {
        assert!(
            census.contains_key(&required),
            "the census never saw the {} chunk — the corpus walk is broken",
            String::from_utf8_lossy(&required)
        );
    }
    assert!(
        app_count >= 30 && beams.len() >= 1000,
        "the census covered only {app_count} application(s) / {} module(s) — expected the full OTP install",
        beams.len()
    );
    // ... and the decoded-term round-trip genuinely ran: `CInf` is present in every
    // installed module and `Meta` in all but one, so a floor well below the real counts
    // still catches a silently skipped check.
    assert!(
        cinf_round_tripped >= 1000 && meta_round_tripped >= 1000,
        "the CInf/Meta round-trip only ran for {cinf_round_tripped}/{meta_round_tripped} module(s) \
         — the decoded-term check is not being exercised"
    );

    // The classification must cover every id: an unclassified id is the regression this
    // checker exists to catch.
    assert!(
        unknown.is_empty(),
        "{} module(s) carried {} unclassified chunk id(s): {unknown:?}",
        unknown.len(),
        unknown.len(),
    );
    assert!(
        failures.is_empty(),
        "{} module(s) failed the census/load (a regression): {failures:?}",
        failures.len()
    );
    assert_eq!(loaded, beams.len(), "not every discovered module loaded");
}

/// The classification table itself must be internally consistent: the ids the corpus
/// relies on are classified exactly as documented.
#[test]
fn the_classification_table_matches_the_documented_categories() {
    let cases: [(ChunkId, ChunkClass); 7] = [
        (ChunkId::ATOM, ChunkClass::ParsedExposed),
        (ChunkId::CODE, ChunkClass::ParsedExposed),
        (ChunkId::ATTR, ChunkClass::ParsedExposed),
        (ChunkId::CINF, ChunkClass::ParsedExposed),
        (ChunkId::META, ChunkClass::ParsedExposed),
        // `StrT` is the one chunk that stays captured-verbatim (the runtime never
        // interprets it), so the `ParsedRaw` category is still exercised.
        (ChunkId::STRT, ChunkClass::ParsedRaw),
        (ChunkId::DOCS, ChunkClass::ToolingOnly),
    ];
    for (id, expected) in cases {
        let kind = classify::classify(id).expect("classified");
        assert_eq!(
            kind.class,
            expected,
            "{} is classified {:?}",
            kind.name,
            kind.class
        );
    }
    // The debug chunks stay tooling-only.
    for id in [ChunkId::DBGB, ChunkId::DBGI, ChunkId::OBSOLETE_ATOM] {
        assert_eq!(classify::classify(id).unwrap().class, ChunkClass::ToolingOnly);
    }
}

// ---------------------------------------------------------------------------
// The `Attr` oracle cross-check
// ---------------------------------------------------------------------------

/// The system Erlang's own `term_to_binary(Module:module_info(attributes))`, or `None`
/// when the oracle is unavailable.
///
/// This is the runtime path the C implements (`erl_bif_info.c:5873-5894`), which
/// decodes the `Attr` chunk verbatim. (`beam_lib:chunks/2`'s `attributes` form must
/// **not** be used: `beam_lib` normalizes the list, moving `vsn` last.)
fn oracle_attribute_bytes(module: &str) -> Option<Vec<u8>> {
    let eval = format!(
        "M=list_to_atom(\"{module}\"),A=M:module_info(attributes),\
         io:format(\"~s\",[binary:encode_hex(term_to_binary(A))]),halt()."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hex = String::from_utf8(output.stdout).ok()?;
    decode_hex(hex.trim())
}

/// Parses an uppercase-or-lowercase hex string into bytes.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// The decoded `Attr` term must equal the oracle's `module_info(attributes)` for two
/// representative modules: the decoded term is re-encoded with the crate's own ETF
/// encoder and compared, byte for byte, against both the raw `Attr` chunk and the
/// system Erlang's `term_to_binary(Module:module_info(attributes))`.
#[test]
fn decoded_attributes_match_the_oracle() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }

    for (app, module) in [("stdlib", "gen_server"), ("kernel", "kernel")] {
        let Some(ebin) = app_ebin(app) else {
            eprintln!("SKIP: {app} ebin not found");
            return;
        };
        let path = ebin.join(format!("{module}.beam"));
        let loaded = beam_loader::load_file(&path)
            .unwrap_or_else(|error| panic!("load {}: {error:?}", path.display()));

        // The file we loaded is the module we think it is.
        assert_eq!(loaded.atom_name(loaded.name), Some(module));

        // The `Attr` chunk must have decoded into a term.
        let decoded = loaded
            .attributes_term()
            .unwrap_or_else(|| panic!("{}: Attr chunk did not decode", path.display()));

        // 1. Re-encoding the decoded term reproduces the chunk's own bytes exactly.
        let reencoded = beam_codec::encode_term(decoded, &loaded.atom_table)
            .unwrap_or_else(|error| panic!("encode attributes of {module}: {error:?}"));
        let raw = loaded
            .attributes
            .as_ref()
            .unwrap_or_else(|| panic!("{}: raw Attr bytes missing", path.display()));
        assert_eq!(
            &reencoded, raw,
            "{module}: re-encoded attributes differ from the raw Attr chunk"
        );

        // 2. The system Erlang's own encoding of module_info(attributes) is identical.
        match oracle_attribute_bytes(module) {
            Some(oracle) => assert_eq!(
                reencoded, oracle,
                "{module}: decoded attributes differ from the oracle's module_info(attributes)"
            ),
            None => eprintln!(
                "SKIP: oracle `erl` unavailable for {module} — decoded term checked against \
                 the raw chunk only"
            ),
        }

        // 3. A concrete, oracle-verified fact: every attribute list carries a `vsn`.
        assert!(
            loaded.attribute_by_name("vsn").is_some(),
            "{module}: decoded attributes carry no `vsn`"
        );
        // A name that is definitely not an attribute is `None`, never a panic.
        assert_eq!(loaded.attribute_by_name("no_such_attribute"), None);
    }
}

// ---------------------------------------------------------------------------
// The `CInf`/`Meta` oracle cross-check
// ---------------------------------------------------------------------------

/// The system Erlang's `term_to_binary(binary_to_term(Chunk))` for the `CInf` and `Meta`
/// chunks of `file`, as hex strings (`"none"` for a chunk the file does not carry), or
/// `None` when the oracle is unavailable.
///
/// The probe mirrors the reader side — `lib/stdlib/src/beam_lib.erl:1071-1077`
/// (`chunk_to_data/6` decodes `compile_info` as `binary_to_term(Chunk)`) and the raw
/// `"Meta"` chunk — so a difference can only mean this crate's decode is wrong. The file
/// path is passed through the environment (`os:getenv/1`) rather than interpolated into
/// the `-eval` source, so a path with quotes or backslashes cannot break the probe.
fn oracle_compile_meta_hex(file: &Path) -> Option<(String, String)> {
    let eval = "F=os:getenv(\"BEAM_ORACLE_FILE\"),\
                {ok,{_,Cs}}=beam_lib:chunks(F,[\"CInf\",\"Meta\"],[allow_missing_chunks]),\
                E=fun(missing_chunk)->\"none\";\
                    (B)->binary:encode_hex(term_to_binary(binary_to_term(B))) end,\
                io:format(\"~s ~s\",[E(proplists:get_value(\"CInf\",Cs,missing_chunk)),\
                                     E(proplists:get_value(\"Meta\",Cs,missing_chunk))]),\
                halt().";
    let output = Command::new("erl")
        .args(["-noshell", "-eval", eval])
        .env("BEAM_ORACLE_FILE", file)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut fields = text.split_whitespace();
    let cinf = fields.next()?.to_string();
    let meta = fields.next()?.to_string();
    Some((cinf, meta))
}

/// The decoded `CInf`/`Meta` terms must equal the oracle's view of the same chunks for
/// two representative modules: each decoded term is re-encoded with the crate's own ETF
/// encoder and compared, byte for byte, against the raw chunk *and* against the system
/// Erlang's `term_to_binary(binary_to_term(Chunk))` (`beam_lib:chunks/2`).
///
/// `CInf` is the value `module_info(compile)` serves (the C decodes it with
/// `erts_decode_ext`: `erts/emulator/beam/erl_bif_info.c:5896-5918`); `Meta` is the
/// compiler's feature metadata (`lib/compiler/src/compile.erl:2274-2295`).
#[test]
fn decoded_compile_info_and_meta_match_the_oracle() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }

    for (app, module) in [("stdlib", "gen_server"), ("kernel", "kernel")] {
        let Some(ebin) = app_ebin(app) else {
            eprintln!("SKIP: {app} ebin not found");
            return;
        };
        let path = ebin.join(format!("{module}.beam"));
        let loaded = beam_loader::load_file(&path)
            .unwrap_or_else(|error| panic!("load {}: {error:?}", path.display()));
        assert_eq!(loaded.atom_name(loaded.name), Some(module));

        // --- `CInf` ---------------------------------------------------------
        let raw = loaded
            .compile_info
            .as_deref()
            .unwrap_or_else(|| panic!("{}: raw CInf bytes missing", path.display()));
        let decoded = loaded
            .compile_info_term()
            .unwrap_or_else(|| panic!("{}: CInf chunk did not decode", path.display()));
        let reencoded_cinf = beam_codec::encode_term(decoded, &loaded.atom_table)
            .unwrap_or_else(|error| panic!("encode CInf of {module}: {error:?}"));
        assert_eq!(
            reencoded_cinf, raw,
            "{module}: re-encoded CInf differs from the raw CInf chunk"
        );

        // Concrete, oracle-verified facts `module_info(compile)` relies on: the compiler
        // writes `[{version,?COMPILER_VSN}|Compile]`, and every build records the source
        // path (`beam_asm.erl:307-310`).
        assert!(
            loaded.compile_info_by_name("version").is_some(),
            "{module}: decoded CInf carries no `version`"
        );
        assert!(
            loaded.compile_info_by_name("source").is_some(),
            "{module}: decoded CInf carries no `source`"
        );
        // A name that is definitely not present is `None`, never a panic.
        assert_eq!(loaded.compile_info_by_name("no_such_key"), None);

        // --- `Meta` ---------------------------------------------------------
        // `Meta` is absent from a handful of installed modules, so the invariant is that
        // the decoded holder is empty exactly when the chunk is absent.
        let raw_meta = loaded.meta.as_deref();
        let decoded_meta = loaded.meta_term();
        let reencoded_meta = match (raw_meta, decoded_meta) {
            (Some(raw), Some(term)) => {
                let reencoded = beam_codec::encode_term(term, &loaded.atom_table)
                    .unwrap_or_else(|error| panic!("encode Meta of {module}: {error:?}"));
                assert_eq!(
                    reencoded, raw,
                    "{module}: re-encoded Meta differs from the raw Meta chunk"
                );
                // The one feature in this OTP release's universe is `maybe_expr`, and the
                // compiler always records it under `enabled_features`
                // (`compile.erl:2269-2289`).
                assert!(
                    loaded.meta_by_name("enabled_features").is_some(),
                    "{module}: decoded Meta carries no `enabled_features`"
                );
                assert_eq!(loaded.meta_by_name("no_such_key"), None);
                Some(reencoded)
            }
            (None, None) => {
                eprintln!("SKIP: {module} carries no Meta chunk");
                None
            }
            (raw, term) => panic!(
                "{module}: Meta chunk/term mismatch: raw={raw:?} term={term:?}"
            ),
        };

        // --- the oracle -----------------------------------------------------
        match oracle_compile_meta_hex(&path) {
            Some((oracle_cinf, oracle_meta)) => {
                let oracle_cinf =
                    decode_hex(&oracle_cinf).expect("the oracle CInf hex must decode");
                assert_eq!(
                    reencoded_cinf, oracle_cinf,
                    "{module}: decoded CInf differs from the oracle's \
                     term_to_binary(binary_to_term(CInf))"
                );
                match (&reencoded_meta, decode_hex(&oracle_meta)) {
                    (Some(ours), Some(oracle)) => assert_eq!(
                        *ours, oracle,
                        "{module}: decoded Meta differs from the oracle's \
                         term_to_binary(binary_to_term(Meta))"
                    ),
                    (None, None) => {}
                    (ours, oracle) => panic!(
                        "{module}: oracle Meta presence disagrees with the loaded module \
                         (ours={ours:?} oracle_len={:?})",
                        oracle.map(|bytes| bytes.len())
                    ),
                }
            }
            None => eprintln!(
                "SKIP: oracle `erl` unavailable for {module} — the decoded terms were checked \
                 against the raw chunks only"
            ),
        }
    }
}
