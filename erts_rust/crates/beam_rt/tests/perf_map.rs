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

//! The `+JPperf` perf-map / JIT-dump surface **end-to-end** (contract A.8).
//!
//! The flag grammar, the bit accumulation, the file paths and the line formatter are
//! unit-tested in `src/perf_map.rs`; this suite proves the **built binary** honours the
//! switches the way the oracle does:
//!
//! * `+JPperf map` creates `<directory>/perf-<pid>.map` (the OS pid of the `beam_rt`
//!   process itself, exactly as the C's `getpid()`), and every line it holds matches
//!   `^0x[0-9a-f]+ [0-9a-f]+ \$.*$` — the grammar of
//!   `fprintf(file, "%p %tx $%s\n", start, size, name)`;
//! * `+JPperf true` additionally creates `<directory>/jit-<pid>.dump`;
//! * `+JPperf false` (and `fp`/`no_fp`, which select no file) creates nothing;
//! * `ERL_SYM_MAP_FILE` replaces the map file's whole path;
//! * a value outside the grammar is a usage error (exit 2 in this runtime; the oracle
//!   exits 1 — see `src/cli/flags.rs`'s documented divergence).
//!
//! The map this runtime writes is **empty**: it executes the typed IR on the interpreter and
//! `beam_engine`'s baseline JIT exposes no native code ranges, and an address is never
//! fabricated. That is pinned explicitly below, and the oracle cross-check (which skips
//! loudly without `erl`) shows the same file path and the same line grammar on the real
//! system's non-empty map.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

// The shared helper module is compiled into every integration-test binary, so a binary that
// does not call all of its entry points would otherwise warn on the unused ones.
#[allow(dead_code)]
mod support;

/// How often a running probe's exit is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A fresh, unique directory under the OS temp dir for one test's perf files.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("beam_rt_perf_map_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    dir
}

/// The files (as names) a directory holds, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the scratch directory is readable")
        .map(|entry| {
            entry
                .expect("a readable entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// The `^0x[0-9a-f]+ [0-9a-f]+ \$.*$` grammar, checked field by field (no regex crate).
fn is_valid_line(line: &str) -> bool {
    let mut fields = line.split(' ');
    let (Some(address), Some(size), Some(name), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    let hex = |text: &str, prefix: &str| {
        text.strip_prefix(prefix).is_some_and(|digits| {
            !digits.is_empty()
                && digits
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    };
    hex(address, "0x")
        && !size.is_empty()
        && size
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && name.starts_with('$')
        && name.len() > 1
}

/// Every non-empty line of `path` must match the oracle's perf-map grammar.
fn assert_valid_map(path: &Path) -> String {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {} failed: {error}", path.display()));
    for line in text.lines() {
        assert!(is_valid_line(line), "{}: {line:?}", path.display());
    }
    text
}

/// Runs `program` with `args`/`envs`, returning the **child's pid** and its output.
///
/// The pid matters: both runtimes name the perf files after `getpid()`, so only the spawner
/// knows where to look (this is why the crate's `tests/support` helper — which never exposes
/// the child — is not used here). The child is polled, so a probe that ignores its
/// `-s init stop` terminator is killed at [`support::PROBE_TIMEOUT`] instead of hanging the
/// suite.
fn run_with_pid(program: &str, args: &[&str], envs: &[(&str, &str)]) -> (u32, Output) {
    let mut command = Command::new(program);
    // Strip every environment variable both runtimes read outside the command line, so each
    // case is exactly what the test sets (a stale ERL_SYM_MAP_FILE would otherwise decide the
    // map's path for us).
    for name in [
        "ERL_AFLAGS",
        "ERL_FLAGS",
        "ERL_ZFLAGS",
        "ERL_SYM_MAP_FILE",
        beam_rt::cli::otp_flags_var().as_str(),
    ] {
        command.env_remove(name);
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child: Child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawning {program} {args:?} failed: {error}"));
    let pid = child.id();
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{program} {args:?} outlived the probe bound");
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => panic!("polling {program} failed: {error}"),
        }
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("collecting {program}'s output failed: {error}"));
    (pid, output)
}

/// The built `beam_rt` binary: `-eval ok` boots, evaluates and exits 0.
fn beam_rt_perf(args: &[&str], envs: &[(&str, &str)]) -> (u32, Output) {
    let mut full = vec!["-noshell"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["-eval", "ok"]);
    run_with_pid(env!("CARGO_BIN_EXE_beam_rt"), &full, envs)
}

/// `beam_rt` with `+JPperf <value>` and `+JPperfdirectory <dir>`.
fn beam_rt_with(value: &str, dir: &Path, envs: &[(&str, &str)]) -> (u32, Output) {
    let dir = dir.to_str().expect("the scratch path is UTF-8");
    beam_rt_perf(&["+JPperf", value, "+JPperfdirectory", dir], envs)
}

// ---------------------------------------------------------------------------------------
// The files the flag selects
// ---------------------------------------------------------------------------------------

/// `+JPperf map` writes `<directory>/perf-<pid>.map`, named after the **runtime's** own OS
/// pid, and every line matches the oracle's grammar.
#[test]
fn plus_jperf_map_writes_the_map_at_the_documented_location() {
    let dir = scratch("map");
    let (pid, output) = beam_rt_with("map", &dir, &[]);
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(
        output.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let map = dir.join(format!("perf-{pid}.map"));
    assert!(map.is_file(), "expected {}, got {:?}", map.display(), entries(&dir));
    assert_valid_map(&map);
    assert_eq!(entries(&dir), [format!("perf-{pid}.map")]);
}

/// The map this runtime writes is **empty**: it produces no JIT-ed machine code and exposes
/// no native ranges, and an address is never fabricated (see `src/perf_map.rs`).
///
/// This is the documented Milestone-1 outcome, pinned so that a future runtime which does
/// feed ranges through `PerfMap::record` has to update it deliberately.
#[test]
fn the_map_is_a_well_formed_empty_file() {
    let dir = scratch("empty");
    let (pid, output) = beam_rt_with("map", &dir, &[]);
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let map = dir.join(format!("perf-{pid}.map"));
    let bytes = std::fs::read(&map).expect("the map is readable");
    assert!(
        bytes.is_empty(),
        "no fabricated addresses: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

/// `+JPperf true` is `map` **and** `dump`: both files appear, and the dump carries the exact
/// 40-byte JITDUMP header of `beam_jit_metadata.cpp` (no `JIT_CODE_LOAD` records).
#[test]
fn plus_jperf_true_writes_both_files() {
    let dir = scratch("true");
    let (pid, output) = beam_rt_with("true", &dir, &[]);
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert_eq!(
        entries(&dir),
        [format!("jit-{pid}.dump"), format!("perf-{pid}.map")]
    );
    assert!(std::fs::read(dir.join(format!("perf-{pid}.map")))
        .expect("the map is readable")
        .is_empty());

    let dump = std::fs::read(dir.join(format!("jit-{pid}.dump"))).expect("the dump is readable");
    assert_eq!(dump.len(), 40, "the header is `4 * 10` bytes (the C's assert)");
    let word = |range: std::ops::Range<usize>| {
        u32::from_le_bytes(dump[range].try_into().expect("4 bytes"))
    };
    assert_eq!(word(0..4), 0x4A69_5444, "the JITDUMP magic");
    assert_eq!(&dump[0..4], b"DTiJ", "the C's native byte order");
    assert_eq!(word(4..8), 1, "version");
    assert_eq!(word(8..12), 40, "total_size");
    assert_eq!(word(12..16), 62, "EM_X86_64");
    assert_eq!(word(16..20), 0, "pad1");
    assert_eq!(word(20..24), pid, "the runtime's own pid");
    assert!(u64::from_le_bytes(dump[24..32].try_into().expect("8 bytes")) > 0);
    assert_eq!(u64::from_le_bytes(dump[32..40].try_into().expect("8 bytes")), 0);
}

/// `false` clears every option, so no file is created — and neither do `dump`-less
/// `fp`/`no_fp`, which only choose code generation.
#[test]
fn plus_jperf_false_writes_nothing() {
    for value in ["false", "fp", "no_fp"] {
        let dir = scratch(&format!("off_{value}"));
        let (_, output) = beam_rt_with(value, &dir, &[]);
        assert!(output.status.success(), "{value}: stderr {:?}", output.stderr);
        assert!(entries(&dir).is_empty(), "{value}: {:?}", entries(&dir));
    }
}

/// The writes are **additive** in command-line order, exactly as the C's bit accumulator:
/// `false` after `map` turns the map off again, and `dump` after it selects only the dump.
#[test]
fn several_occurrences_accumulate_in_command_line_order() {
    let dir = scratch("accumulate");
    let dir_str = dir.to_str().expect("UTF-8").to_string();
    let (pid, output) = beam_rt_perf(
        &[
            "+JPperf",
            "map",
            "+JPperf",
            "dump",
            "+JPperfdirectory",
            &dir_str,
        ],
        &[],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert_eq!(
        entries(&dir),
        [format!("jit-{pid}.dump"), format!("perf-{pid}.map")]
    );

    let off = scratch("accumulate_off");
    let off_str = off.to_str().expect("UTF-8").to_string();
    let (_, output) = beam_rt_perf(
        &[
            "+JPperf",
            "map",
            "+JPperf",
            "false",
            "+JPperfdirectory",
            &off_str,
        ],
        &[],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(entries(&off).is_empty(), "{:?}", entries(&off));
}

/// `ERL_SYM_MAP_FILE` replaces the map file's whole path (the emulator reads it directly,
/// outside the `ERL_*FLAGS` merge) and leaves the dump path alone.
#[test]
fn the_sym_map_file_variable_overrides_the_whole_map_path() {
    let dir = scratch("sym_map_file");
    let override_path = dir.join("custom.map");
    let override_str = override_path.to_str().expect("UTF-8");
    let (pid, output) = beam_rt_perf(
        &["+JPperf", "true", "+JPperfdirectory", dir.to_str().expect("UTF-8")],
        &[("ERL_SYM_MAP_FILE", override_str)],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(
        override_path.is_file(),
        "expected {}, got {:?}",
        override_path.display(),
        entries(&dir)
    );
    assert_valid_map(&override_path);
    // The dump file ignores the override, as the C does.
    assert_eq!(entries(&dir), ["custom.map", &format!("jit-{pid}.dump")]);
}

/// `ERL_SYM_MAP_FILE` selects no file of its own: with everything off nothing is written.
#[test]
fn the_sym_map_file_variable_selects_nothing_on_its_own() {
    let dir = scratch("sym_map_file_off");
    let override_path = dir.join("custom.map");
    let (_, output) = beam_rt_perf(
        &["+JPperfdirectory", dir.to_str().expect("UTF-8")],
        &[("ERL_SYM_MAP_FILE", override_path.to_str().expect("UTF-8"))],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(entries(&dir).is_empty(), "{:?}", entries(&dir));
}

/// A value outside the grammar — and a missing value — is this runtime's **usage** error
/// (exit 2 + the usage text), never a silent default and never an accepted no-op.
#[test]
fn a_malformed_flag_is_a_usage_error() {
    for args in [
        &["+JPperf", "bogus"][..],
        &["+JPperf"][..],
        &["+JPperfdirectory"][..],
        &["+JPperf", "MAP"][..],
    ] {
        let (_, output) = beam_rt_perf(args, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Usage:"), "{args:?}: {stderr}");
    }
    // A directory that cannot be created is **not** a usage error: the runtime records it and
    // carries on, exactly as the oracle logs it and exits 0.
    let missing = scratch("missing").join("no_such_subdirectory");
    let (_, output) = beam_rt_perf(
        &["+JPperf", "map", "+JPperfdirectory", missing.to_str().expect("UTF-8")],
        &[],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(!missing.exists());
}

// ---------------------------------------------------------------------------------------
// The parser surface (the CLI wiring, exercised through the library)
// ---------------------------------------------------------------------------------------

/// The two switches are **emulator** flags: they are preserved verbatim in
/// `extra_emulator_flags` (nothing is lost, contract A.6) and are never `init` arguments.
#[test]
fn the_switches_are_emulator_flags_not_init_arguments() {
    let config = beam_rt::parse(&[
        "+JPperf".to_string(),
        "map".to_string(),
        "+JPperfdirectory".to_string(),
        "/var/perf".to_string(),
    ])
    .expect("the flags are accepted");
    assert!(config.perf_map.map_enabled());
    assert!(!config.perf_map.dump_enabled());
    assert!(config.perf_map.frame_pointers());
    assert_eq!(config.perf_map.directory(), Path::new("/var/perf"));
    assert_eq!(
        config.emulator.extra_emulator_flags,
        ["+JPperf", "map", "+JPperfdirectory", "/var/perf"]
    );
    for name in ["JPperf", "JPperfdirectory"] {
        assert!(
            config.init_args.get(name).is_none(),
            "{name} must not be an init argument"
        );
    }
}

/// `ERL_SYM_MAP_FILE` is read by the environment-aware entry point (the pure `parse` never
/// touches the environment) and is not an `init` argument either.
#[test]
fn the_environment_entry_point_reads_the_map_file_variable() {
    let env = beam_rt::cli::MapEnv(std::collections::HashMap::from([(
        "ERL_SYM_MAP_FILE".to_string(),
        "/somewhere/else.map".to_string(),
    )]));
    let args = vec!["+JPperf".to_string(), "map".to_string()];
    let config = beam_rt::cli::parse_with_env(&args, &env).expect("the flags are accepted");
    assert_eq!(config.perf_map.file(), Some(Path::new("/somewhere/else.map")));
    assert!(config.perf_map.map_enabled());
    assert!(config.init_args.get("SYM_MAP_FILE").is_none());

    // The pure parser sees no environment: the override stays unset.
    let pure = beam_rt::parse(&args).expect("the flags are accepted");
    assert_eq!(pure.perf_map.file(), None);
}

/// A `Runtime` built from the parsed configuration owns the perf map: the file exists as soon
/// as the runtime is constructed (the emulator-start equivalent), and `record` appends to it.
#[test]
fn the_runtime_owns_the_perf_map_and_records_through_it() {
    let dir = scratch("runtime");
    let mut config = beam_rt::config::RuntimeConfig::default();
    let mut perf = beam_rt::perf_map::PerfMapConfig::new();
    perf.set_directory(&dir);
    perf.set_flag("map").expect("map is a grammar value");
    config = config.with_perf_map(perf);

    let mut runtime = beam_rt::Runtime::new(config).expect("the runtime is created");
    let path = runtime
        .perf_map()
        .map_path()
        .expect("the map file is open")
        .to_path_buf();
    assert_eq!(
        path,
        dir.join(format!("perf-{}.map", std::process::id()))
    );
    assert_eq!(runtime.perf_map().error(), None);
    assert_eq!(runtime.perf_map().entry_count(), 0);

    runtime
        .perf_map_mut()
        .record(0x4011_20, 0x30, "some_module:function/2")
        .expect("recording a range");
    assert_eq!(runtime.perf_map().entry_count(), 1);
    let text = std::fs::read_to_string(&path).expect("the map is readable");
    assert_eq!(text, "0x401120 30 $some_module:function/2\n");
    assert!(text.lines().all(is_valid_line), "{text:?}");
}

// ---------------------------------------------------------------------------------------
// The oracle cross-check
// ---------------------------------------------------------------------------------------

/// Both runtimes create the map at the same documented path with the same line grammar; the
/// oracle's map is non-empty (it has JIT ranges) and this runtime's is empty (it has none).
///
/// The oracle probe is `-noshell +JPperf <value> -eval ok -s init stop`, with
/// `ERL_SYM_MAP_FILE` pinning the path (the oracle's own pid is the launcher's `exec`-ed
/// process, which a test cannot rely on). Skips **loudly** when there is no `erl`.
#[test]
fn the_map_location_and_line_grammar_match_the_oracle() {
    let Some(oracle) = support::require_oracle("perf_map") else {
        return;
    };
    let oracle = oracle.to_str().expect("the oracle path is UTF-8").to_string();

    // `+JPperf map` — the oracle writes a non-empty, grammar-valid map at our exact path.
    let dir = scratch("oracle");
    let oracle_map = dir.join("oracle.map");
    let oracle_map_str = oracle_map.to_str().expect("UTF-8");
    let (_, output) = run_with_pid(
        &oracle,
        &["-noshell", "+JPperf", "map", "-eval", "ok", "-s", "init", "stop"],
        &[("ERL_SYM_MAP_FILE", oracle_map_str)],
    );
    assert!(
        output.status.success(),
        "the oracle exited with {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(oracle_map.is_file(), "the oracle wrote no map");
    let text = assert_valid_map(&oracle_map);
    assert!(!text.is_empty(), "the oracle's map names its JIT ranges");

    // The runtime under test: same path, same grammar, empty map.
    let (_, output) = beam_rt_perf(
        &["+JPperf", "map"],
        &[("ERL_SYM_MAP_FILE", oracle_map_str)],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert_valid_map(&oracle_map);
    assert!(
        std::fs::read(&oracle_map)
            .expect("the map is readable")
            .is_empty(),
        "beam_rt fabricates no addresses"
    );

    // `+JPperf false` writes nothing in either runtime.
    let off_map = dir.join("off.map");
    let off_map_str = off_map.to_str().expect("UTF-8");
    let (_, output) = run_with_pid(
        &oracle,
        &["-noshell", "+JPperf", "false", "-eval", "ok", "-s", "init", "stop"],
        &[("ERL_SYM_MAP_FILE", off_map_str)],
    );
    assert!(output.status.success(), "the oracle exited non-zero");
    assert!(!off_map.exists(), "the oracle wrote a map with `false`");
    let (_, output) = beam_rt_perf(
        &["+JPperf", "false"],
        &[("ERL_SYM_MAP_FILE", off_map_str)],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(!off_map.exists(), "beam_rt wrote a map with `false`");
}
