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

//! Perf-map / JIT symbol dumping (contract A.8): the `+JPperf`-gated
//! `<directory>/perf-<pid>.map` writer (the `<start> <size> <name>` line grammar the Linux
//! `perf` tooling reads), plus the `jit-<pid>.dump` file the `dump` option selects.
//!
//! # Ground truth
//!
//! * **The flags** — `erts/emulator/beam/erl_init.c:1732-1775` (the `+JPperf` value grammar,
//!   whose options *accumulate* option by option, and `+JPperfdirectory`) and
//!   `erts/doc/references/erl_cmd.md` (`+JPperf`, `+JPperfdirectory`). The default directory is
//!   `/tmp` (`erts/emulator/beam/jit/beam_jit_main.cpp:47`:
//!   `char etrs_jit_perf_directory[MAXPATHLEN] = "/tmp"`).
//! * **The writers** — `erts/emulator/beam/jit/beam_jit_metadata.cpp`: `JitPerfMap`
//!   (`<directory>/perf-<pid>.map`, or the **whole** path from `ERL_SYM_MAP_FILE`) and
//!   `JitPerfDump` (`<directory>/jit-<pid>.dump`). Both files are created when the emulator
//!   starts (`beamasm_metadata_early_init`) and appended to as each module is JIT-compiled.
//! * **The map line grammar** — `fprintf(file, "%p %tx $%s\n", start, size, name)`, i.e.
//!   `0x<lowercase-hex-address> <lowercase-hex-size> $<symbol>`, one line per code range:
//!
//!   ```text
//!   0x77b2eac00040 68 $global::apply_fun_shared
//!   0x77b2eb063320 168 $erl_internal:'-predefined_functions/1-lc$^0/1-0-'/1
//!   0x77b2eb063488 4b0 $erl_internal::codeFooter
//!   ```
//!
//!   (Observed on OTP 29.1.1 / erts-17.1 with `+JPperf map`; the sizes are the hex widths the
//!   C's `%tx` produces.)
//!
//! # What this runtime writes, and what it does not
//!
//! The flag surface is modelled **exactly**: every value of the grammar is accepted and
//! accumulated in the C's order, the directory and the `ERL_SYM_MAP_FILE` override are
//! honoured, and the files are created at the documented location the moment the runtime is
//! constructed ([`Runtime::new`](crate::Runtime::new) — the emulator-start equivalent of the
//! C's `beamasm_metadata_early_init`).
//!
//! Every line the map holds is a **real** native code range — an address is **never**
//! fabricated. A **scheduled** process's first module-export entry is the one site that feeds
//! the map: the runner snapshots that module's cached baseline artifacts
//! (`beam_engine::jit::cache::baseline_code_map`, which compiles the module into the
//! per-thread baseline cache on first sight) into the map through [`PerfMap::record`], once
//! per first entry, so each line names code the baseline backend actually mapped. The
//! snapshot is taken for every such entry, whatever execution tier the slice itself runs on.
//! The map is **empty** in exactly the cases where no such entry happens: `+JPperf` is off (no
//! file is opened at all), or the run is a single-pass action (`-eval`/`-s`/`-run`) that
//! never enters a module export through the runner. The `jit-<pid>.dump` file carries the
//! exact 40-byte JITDUMP header and no `JIT_CODE_LOAD`/`JIT_CODE_DEBUG_INFO` record (the dump
//! is not fed by the runner).
//!
//! # Where and when
//!
//! | `+JPperf` value | file |
//! |---|---|
//! | `map` | `<directory>/perf-<pid>.map` (the baseline JIT's real code ranges, appended once per module-export first entry; empty when no such entry happens) |
//! | `dump` | `<directory>/jit-<pid>.dump` (the 40-byte header, no records) |
//! | `true` | both (`true` is `map` **and** `dump`) |
//! | `fp` / `no_fp` | neither — frame pointers are a code-generation choice, and select no file |
//! | `false` | neither (every option is cleared) |
//!
//! `<pid>` is **this process's** OS pid (the C's `getpid()`) and `<directory>` is `/tmp`
//! unless `+JPperfdirectory` set it. `ERL_SYM_MAP_FILE` replaces the map file's **whole**
//! path; the dump path ignores it, exactly as in the C.
//!
//! A file that cannot be created (an unwritable directory, say) is **recorded** — never
//! raised and never printed — and the run carries on, which is what the oracle does (it logs
//! to the error logger, a process this runtime does not have; the reason is observable
//! through [`PerfMap::error`]). Failing the run instead would diverge from the oracle, which
//! exits 0 on an unopenable perf file.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use beam_common::Result;

use crate::error;

/// The environment variable that overrides the **whole** map-file path
/// (`beam_jit_metadata.cpp`'s `JitPerfMap::init`, read through the emulator's
/// `erts_sys_explicit_host_getenv` — i.e. outside the `ERL_*FLAGS` merge, and not an `init`
/// argument).
pub const SYM_MAP_FILE_VAR: &str = "ERL_SYM_MAP_FILE";

/// The default directory the perf files are written to (`beam_jit_main.cpp:47`).
pub const DEFAULT_DIRECTORY: &str = "/tmp";

/// `EM_X86_64`, the JITDUMP header's `elf_mach` field (`beam_jit_metadata.cpp:305`).
const ELF_MACH_X86_64: u32 = 62;

/// The JITDUMP header's `magic` (`0x4A695444`, "JiTD as numbers";
/// `beam_jit_metadata.cpp:303`). The C `fwrite`s the `u32` in native byte order, so the bytes
/// on x86-64 are `44 54 69 4A` — which is exactly what [`dump_header`] reproduces.
const JITDUMP_MAGIC: u32 = 0x4A69_5444;

/// The JITDUMP header `version` (`beam_jit_metadata.cpp:304`).
const JITDUMP_VERSION: u32 = 1;

/// The JITDUMP `FileHeader` size in bytes — the C asserts `sizeof(FileHeader) == 4 * 10`
/// (`beam_jit_metadata.cpp:315`).
const JITDUMP_HEADER_SIZE: u32 = 40;

/// One value of the `+JPperf` grammar (`erl_init.c:1748-1770`).
///
/// The values are **not** mutually exclusive modes: each one sets or clears bits of the
/// emulator's `erts_jit_perf_support` accumulator, so several occurrences combine in command
/// line order (`beam_asm.h:42-48`: `DUMP = 1<<0`, `MAP = 1<<1`, `FP = 1<<2`,
/// `ENABLED = DUMP|MAP|FP`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfFlag {
    /// `true` — every option (`ENABLED`: map, dump and frame pointers).
    True,
    /// `false` — clear every option.
    False,
    /// `dump` — detailed line information, so `perf annotate` works.
    Dump,
    /// `map` — the module-code map; also enables frame pointers.
    Map,
    /// `fp` — frame pointers, independently of `map`.
    FramePointers,
    /// `no_fp` — disable the frame pointers `map` added.
    NoFramePointers,
}

impl PerfFlag {
    /// Every accepted value, in the grammar's order.
    pub const ALL: [PerfFlag; 6] = [
        PerfFlag::True,
        PerfFlag::False,
        PerfFlag::Dump,
        PerfFlag::Map,
        PerfFlag::FramePointers,
        PerfFlag::NoFramePointers,
    ];

    /// Parses a `+JPperf` value; `None` for anything outside the grammar.
    ///
    /// The comparison is exact, as in the C (`sys_strcmp`), so `True`/`MAP` are unknown
    /// values, never case-folded aliases.
    pub fn parse(value: &str) -> Option<PerfFlag> {
        PerfFlag::ALL.into_iter().find(|flag| flag.name() == value)
    }

    /// The value's spelling, as the oracle's help text prints it.
    pub fn name(self) -> &'static str {
        match self {
            PerfFlag::True => "true",
            PerfFlag::False => "false",
            PerfFlag::Dump => "dump",
            PerfFlag::Map => "map",
            PerfFlag::FramePointers => "fp",
            PerfFlag::NoFramePointers => "no_fp",
        }
    }
}

/// The parsed `+JPperf` / `+JPperfdirectory` / `ERL_SYM_MAP_FILE` state.
///
/// The three bits mirror the C's `erts_jit_perf_support` (`map`, `dump`, `frame_pointers`),
/// the directory mirrors `etrs_jit_perf_directory` and the file overrides the map path
/// exactly as `ERL_SYM_MAP_FILE` does. The default is "every option off, `/tmp`, no
/// override": a bare invocation writes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfMapConfig {
    /// The `BEAMASM_PERF_MAP` bit: write `<directory>/perf-<pid>.map`.
    map: bool,
    /// The `BEAMASM_PERF_DUMP` bit: write `<directory>/jit-<pid>.dump`.
    dump: bool,
    /// The `BEAMASM_PERF_FP` bit: emit frame pointers in JIT-ed code (no file of its own).
    frame_pointers: bool,
    /// `etrs_jit_perf_directory`: where the files go (default [`DEFAULT_DIRECTORY`]).
    directory: PathBuf,
    /// `ERL_SYM_MAP_FILE`: when set, the map file's whole path.
    file: Option<PathBuf>,
}

impl Default for PerfMapConfig {
    fn default() -> Self {
        PerfMapConfig {
            map: false,
            dump: false,
            frame_pointers: false,
            directory: PathBuf::from(DEFAULT_DIRECTORY),
            file: None,
        }
    }
}

impl PerfMapConfig {
    /// The default configuration (same as [`Default::default`]).
    pub fn new() -> Self {
        PerfMapConfig::default()
    }

    /// Applies one `+JPperf <value>` occurrence, accumulating exactly as the C does.
    ///
    /// A value outside the grammar is a usage error ([`crate::error`]'s
    /// `bad_switch_value`), which [`crate::cli`] reports as exit 2 — the crate's uniform
    /// contract for a malformed emulator value (the oracle prints `bad +JPperf support flag
    /// <value>` and exits 1).
    ///
    /// # Errors
    ///
    /// The usage error above.
    pub fn set_flag(&mut self, value: &str) -> Result<()> {
        let flag =
            PerfFlag::parse(value).ok_or_else(|| error::bad_switch_value("+JPperf", value))?;
        match flag {
            PerfFlag::True => {
                self.map = true;
                self.dump = true;
                self.frame_pointers = true;
            }
            PerfFlag::False => {
                self.map = false;
                self.dump = false;
                self.frame_pointers = false;
            }
            PerfFlag::Dump => self.dump = true,
            PerfFlag::Map => {
                self.map = true;
                self.frame_pointers = true;
            }
            PerfFlag::FramePointers => self.frame_pointers = true,
            PerfFlag::NoFramePointers => self.frame_pointers = false,
        }
        Ok(())
    }

    /// Applies `+JPperfdirectory <directory>`.
    ///
    /// The value is taken verbatim (no validation, no canonicalisation): the C `strncpy`s it
    /// into a fixed buffer, so an unusable directory surfaces later, as a failed file
    /// creation ([`PerfMap::error`]), never as a usage error.
    pub fn set_directory(&mut self, directory: impl Into<PathBuf>) {
        self.directory = directory.into();
    }

    /// Applies the `ERL_SYM_MAP_FILE` override (the map file's whole path).
    pub fn set_file(&mut self, file: impl Into<PathBuf>) {
        self.file = Some(file.into());
    }

    /// Whether the `map` option is on.
    pub fn map_enabled(&self) -> bool {
        self.map
    }

    /// Whether the `dump` option is on.
    pub fn dump_enabled(&self) -> bool {
        self.dump
    }

    /// Whether the `fp` option is on.
    pub fn frame_pointers(&self) -> bool {
        self.frame_pointers
    }

    /// The directory the perf files are written to.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The `ERL_SYM_MAP_FILE` override, when one was set.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// The path the **map** is written to: the `ERL_SYM_MAP_FILE` override verbatim, else
    /// `<directory>/perf-<pid>.map` (`beam_jit_metadata.cpp:478-490`).
    pub fn map_path(&self, pid: u32) -> PathBuf {
        match &self.file {
            Some(path) => path.clone(),
            None => self.directory.join(format!("perf-{pid}.map")),
        }
    }

    /// The path the **dump** is written to: `<directory>/jit-<pid>.dump`.
    ///
    /// `ERL_SYM_MAP_FILE` deliberately does not apply — the C reads it only in
    /// `JitPerfMap::init` (`beam_jit_metadata.cpp:327-347`), never in `JitPerfDump::init`.
    pub fn dump_path(&self, pid: u32) -> PathBuf {
        self.directory.join(format!("jit-{pid}.dump"))
    }

    /// Whether any option selects a file (i.e. whether [`PerfMap::new`] creates one).
    pub fn writes_files(&self) -> bool {
        self.map || self.dump
    }
}

/// One line of the perf map, exactly as the C's `fprintf(file, "%p %tx $%s\n", ...)` writes
/// it: `0x<hex-address> <hex-size> $<name>`, newline-terminated.
///
/// `name` is the symbol **without** its `$` sigil (the C's `range.name`: `global::stub`,
/// `erl_internal::codeFooter`, `mymod:fun/1`).
pub fn format_line(start: usize, size: usize, name: &str) -> String {
    format!("0x{start:x} {size:x} ${name}\n")
}

/// The 40-byte JITDUMP `FileHeader` (`beam_jit_metadata.cpp:299-317`), in the native
/// little-endian field order the C's `fwrite` produces on x86-64.
///
/// The C's timestamp is `erts_os_monotonic_time()` — nanoseconds on an arbitrary epoch. This
/// runtime has no such clock, so the field carries this process's real UNIX-epoch
/// nanoseconds: a genuine nanosecond timestamp (which is all `perf` uses it for, ordering)
/// and never a fabricated one. A clock before the UNIX epoch yields 0.
fn dump_header(pid: u32) -> [u8; JITDUMP_HEADER_SIZE as usize] {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX));
    let mut header = [0u8; JITDUMP_HEADER_SIZE as usize];
    header[0..4].copy_from_slice(&JITDUMP_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&JITDUMP_VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&JITDUMP_HEADER_SIZE.to_le_bytes());
    header[12..16].copy_from_slice(&ELF_MACH_X86_64.to_le_bytes());
    // `pad1` stays 0.
    header[20..24].copy_from_slice(&pid.to_le_bytes());
    header[24..32].copy_from_slice(&timestamp.to_le_bytes());
    // `flags` stays 0.
    header
}

/// One open perf file and the path it was opened at.
#[derive(Debug)]
struct Sink {
    /// Where the file actually is (the resolved path, not the requested one).
    path: PathBuf,
    /// The open file. Every write is an immediate `write(2)`, so there is nothing to flush.
    file: File,
}

/// The live `+JPperf` files of one runtime.
///
/// [`PerfMap::new`] creates them (truncating, like the C's `fopen(name, "w")`) and never
/// fails: a file that cannot be created is left closed with its reason in
/// [`PerfMap::error`], and the runtime carries on — the oracle's behaviour, whose error
/// logger this runtime does not have. [`PerfMap::record`] is the one writer of a map line.
#[derive(Debug)]
pub struct PerfMap {
    /// The configuration the files were created from.
    config: PerfMapConfig,
    /// The map file, when the `map` option is on and it could be created.
    map: Option<Sink>,
    /// The dump file, when the `dump` option is on and it could be created.
    dump: Option<Sink>,
    /// Why a requested file is missing, when one could not be created.
    error: Option<String>,
    /// How many map lines have been written.
    entries: usize,
}

impl PerfMap {
    /// Creates the files the configuration selects (and nothing when it selects none).
    ///
    /// `<pid>` is this process's OS pid, exactly as the C's `getpid()`. A failed creation is
    /// recorded in [`PerfMap::error`], never raised: the oracle logs it and keeps running.
    pub fn new(config: PerfMapConfig) -> PerfMap {
        let pid = std::process::id();
        let mut perf = PerfMap {
            config,
            map: None,
            dump: None,
            error: None,
            entries: 0,
        };
        if perf.config.map_enabled() {
            let path = perf.config.map_path(pid);
            match File::create(&path) {
                Ok(file) => perf.map = Some(Sink { path, file }),
                Err(error) => perf.note_error(&path, &error),
            }
        }
        if perf.config.dump_enabled() {
            let path = perf.config.dump_path(pid);
            let header = dump_header(pid);
            match File::create(&path).and_then(|mut file| file.write_all(&header).map(|()| file)) {
                Ok(file) => perf.dump = Some(Sink { path, file }),
                Err(error) => perf.note_error(&path, &error),
            }
        }
        perf
    }

    /// Records why a requested perf file could not be created (the oracle's
    /// `perf: Failed to open <path> (<errno>)` logger message, kept in-process).
    fn note_error(&mut self, path: &Path, error: &std::io::Error) {
        self.error = Some(format!("perf: failed to open {}: {error}", path.display()));
    }

    /// The configuration the files were created from.
    pub fn config(&self) -> &PerfMapConfig {
        &self.config
    }

    /// Whether at least one perf file is open (i.e. lines can be written).
    pub fn is_active(&self) -> bool {
        self.map.is_some() || self.dump.is_some()
    }

    /// The path of the open **map** file, when one was created.
    ///
    /// The *requested* path is [`PerfMapConfig::map_path`] (which answers even when the file
    /// could not be created).
    pub fn map_path(&self) -> Option<&Path> {
        self.map.as_ref().map(|sink| sink.path.as_path())
    }

    /// The path of the open **dump** file, when one was created.
    pub fn dump_path(&self) -> Option<&Path> {
        self.dump.as_ref().map(|sink| sink.path.as_path())
    }

    /// Why a requested perf file is missing, when one could not be created.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Records a diagnostic reason for a perf-map operation that did not complete, without
    /// failing the caller.
    ///
    /// The perf map is diagnostic: the C logs a problem and the emulator carries on (exits 0),
    /// so a caller holding a real range that could not be appended (a write failure, say)
    /// reports it here -- observable through [`PerfMap::error`] -- instead of failing the run.
    pub fn note_diagnostic(&mut self, reason: impl Into<String>) {
        self.error = Some(reason.into());
    }

    /// How many map lines have been written.
    pub fn entry_count(&self) -> usize {
        self.entries
    }

    /// Appends one code range to the perf map and returns its line count.
    ///
    /// This is the single writer of the map's line grammar ([`format_line`]). The runner calls
    /// it once per module-export first entry (`runner::perf::record_baseline_code`) with the
    /// baseline JIT's real cached `(start, size, name)` triples, so each line is the oracle's
    /// exact line and never a fabricated address (see the module docs).
    ///
    /// # Errors
    ///
    /// A message error when the map is not open (the flag is off, or the file could not be
    /// created) or when the write fails — a dropped line is never silent.
    pub fn record(&mut self, start: usize, size: usize, name: &str) -> Result<()> {
        let Some(sink) = self.map.as_mut() else {
            return Err(error::message(
                "the perf map is not open: `+JPperf map` is off or its file could not be created",
            ));
        };
        sink.file
            .write_all(format_line(start, size, name).as_bytes())
            .map_err(|error| {
                error::message(format!(
                    "perf: failed to write {}: {error}",
                    sink.path.display()
                ))
            })?;
        self.entries += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique directory under the OS temp dir for one test's perf files.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_perf_map_unit_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        dir
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
            text.strip_prefix(prefix)
                .is_some_and(|digits| {
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

    /// The grammar values parse exactly (case-sensitively), and an unknown value is `None`.
    #[test]
    fn every_grammar_value_parses_and_nothing_else_does() {
        for flag in PerfFlag::ALL {
            assert_eq!(PerfFlag::parse(flag.name()), Some(flag));
        }
        for unknown in ["True", "MAP", "", "map ", "no-fp", "false "] {
            assert_eq!(PerfFlag::parse(unknown), None, "{unknown:?}");
        }
    }

    /// The options accumulate bit by bit in command-line order, exactly as the C does
    /// (`erl_init.c:1748-1770`): `map` implies frame pointers, `no_fp` clears them, `false`
    /// clears everything and `true` sets everything.
    #[test]
    fn the_options_accumulate_like_the_c_bit_flags() {
        let config = |values: &[&str]| {
            let mut config = PerfMapConfig::new();
            for value in values {
                config.set_flag(value).expect("a grammar value");
            }
            (
                config.map_enabled(),
                config.dump_enabled(),
                config.frame_pointers(),
            )
        };
        // The default writes nothing at all.
        assert_eq!(
            (PerfMapConfig::new().map_enabled(), PerfMapConfig::new().dump_enabled()),
            (false, false)
        );
        assert!(!PerfMapConfig::new().writes_files());
        // `dump` alone selects only the dump file.
        assert_eq!(config(&["dump"]), (false, true, false));
        // `map` selects the map file *and* frame pointers.
        assert_eq!(config(&["map"]), (true, false, true));
        // `fp`/`no_fp` are code-generation choices and select no file on their own.
        assert_eq!(config(&["fp"]), (false, false, true));
        assert_eq!(config(&["map", "no_fp"]), (true, false, false));
        assert_eq!(config(&["no_fp", "fp"]), (false, false, true));
        // `true` is every option, `false` clears every option.
        assert_eq!(config(&["true"]), (true, true, true));
        assert_eq!(config(&["true", "false"]), (false, false, false));
        assert_eq!(config(&["false", "map"]), (true, false, true));
        assert_eq!(config(&["map", "false", "dump"]), (false, true, false));
    }

    /// A value outside the grammar is the crate's usage error, never a silent default.
    #[test]
    fn a_malformed_value_is_rejected() {
        let mut config = PerfMapConfig::new();
        assert!(config.set_flag("bogus").is_err());
        assert!(!config.map_enabled() && !config.dump_enabled());
        assert_eq!(PerfMapConfig::new(), config);
    }

    /// The paths are the oracle's: `/tmp/perf-<pid>.map` by default, `<dir>/perf-<pid>.map`
    /// with a directory, and `ERL_SYM_MAP_FILE` verbatim for the map — while the dump path
    /// ignores the override (the C reads it only in `JitPerfMap`).
    #[test]
    fn the_file_paths_match_the_oracle() {
        let default = PerfMapConfig::new();
        assert_eq!(default.directory(), Path::new(DEFAULT_DIRECTORY));
        assert_eq!(default.map_path(4242), PathBuf::from("/tmp/perf-4242.map"));
        assert_eq!(default.dump_path(4242), PathBuf::from("/tmp/jit-4242.dump"));
        assert_eq!(default.file(), None);

        let mut directed = PerfMapConfig::new();
        directed.set_directory("/var/perf");
        assert_eq!(
            directed.map_path(7),
            PathBuf::from("/var/perf/perf-7.map")
        );
        assert_eq!(directed.dump_path(7), PathBuf::from("/var/perf/jit-7.dump"));

        let mut overridden = directed.clone();
        overridden.set_file("/somewhere/else.map");
        assert_eq!(overridden.file(), Some(Path::new("/somewhere/else.map")));
        assert_eq!(overridden.map_path(7), PathBuf::from("/somewhere/else.map"));
        assert_eq!(overridden.dump_path(7), PathBuf::from("/var/perf/jit-7.dump"));
    }

    /// The line grammar is byte-for-byte the oracle's, including the lowercase hex widths
    /// `%p`/`%tx` produce (the three sample lines from the module docs).
    #[test]
    fn the_line_grammar_matches_the_oracle_samples() {
        assert_eq!(
            format_line(0x77b2_eac0_0040, 0x68, "global::apply_fun_shared"),
            "0x77b2eac00040 68 $global::apply_fun_shared\n"
        );
        assert_eq!(
            format_line(
                0x77b2_eb06_3320,
                0xa8,
                "erl_internal:'-predefined_functions/1-lc$^0/1-0-'/1"
            ),
            "0x77b2eb063320 a8 $erl_internal:'-predefined_functions/1-lc$^0/1-0-'/1\n"
        );
        assert_eq!(
            format_line(0x77b2_eb06_3488, 0x4b0, "erl_internal::codeFooter"),
            "0x77b2eb063488 4b0 $erl_internal::codeFooter\n"
        );
        // Low addresses and sizes stay lowercase and unpadded.
        assert_eq!(format_line(0, 0, "x"), "0x0 0 $x\n");
        for line in [
            format_line(0xdead_beef, 0x10, "m:f/0"),
            format_line(1, 250, "global::stub"),
        ] {
            assert!(is_valid_line(line.trim_end()), "{line:?}");
        }
    }

    /// `map` creates a well-formed empty file at the documented location, and `record`
    /// appends lines that parse back against the grammar.
    #[test]
    fn the_map_file_is_created_and_records_the_documented_lines() {
        let dir = scratch("map");
        let mut config = PerfMapConfig::new();
        config.set_directory(&dir);
        config.set_flag("map").expect("map is a grammar value");
        let mut perf = PerfMap::new(config);
        assert!(perf.is_active(), "error: {:?}", perf.error());
        assert_eq!(perf.error(), None);
        assert_eq!(perf.entry_count(), 0);

        let path = perf.map_path().expect("the map file is open").to_path_buf();
        assert_eq!(
            path,
            dir.join(format!("perf-{}.map", std::process::id())),
            "the file must be <directory>/perf-<pid>.map"
        );
        // Created, but empty: this unit test records nothing (a scheduled process's
        // first module-export entry is what feeds real ranges through `PerfMap::record`).
        assert_eq!(std::fs::read(&path).expect("the map is readable"), b"");

        perf.record(0x1000, 0x20, "mymod:fun/1")
            .expect("recording a line");
        perf.record(0xab_cd, 0x4, "global::stub")
            .expect("recording a line");
        assert_eq!(perf.entry_count(), 2);
        let text = std::fs::read_to_string(&path).expect("the map is readable");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(is_valid_line(line), "{line:?}");
        }
        assert_eq!(lines[0], "0x1000 20 $mymod:fun/1");
        assert_eq!(lines[1], "0xabcd 4 $global::stub");
    }

    /// `true` writes both files; the dump is exactly the 40-byte JITDUMP header of the C.
    #[test]
    fn true_writes_both_files_and_the_dump_header_is_exact() {
        let dir = scratch("true");
        let mut config = PerfMapConfig::new();
        config.set_directory(&dir);
        config.set_flag("true").expect("true is a grammar value");
        let perf = PerfMap::new(config);
        assert_eq!(perf.error(), None);
        let pid = std::process::id();

        let map = std::fs::read(dir.join(format!("perf-{pid}.map"))).expect("the map exists");
        assert!(map.is_empty(), "no fabricated addresses");
        let dump = std::fs::read(dir.join(format!("jit-{pid}.dump"))).expect("the dump exists");
        assert_eq!(dump.len(), JITDUMP_HEADER_SIZE as usize);
        let word = |range: std::ops::Range<usize>| {
            u32::from_le_bytes(dump[range].try_into().expect("4 bytes"))
        };
        assert_eq!(word(0..4), JITDUMP_MAGIC);
        assert_eq!(&dump[0..4], b"DTiJ", "the C's native byte order");
        assert_eq!(word(4..8), JITDUMP_VERSION);
        assert_eq!(word(8..12), JITDUMP_HEADER_SIZE);
        assert_eq!(word(12..16), ELF_MACH_X86_64);
        assert_eq!(word(16..20), 0, "pad1");
        assert_eq!(word(20..24), pid);
        assert!(u64::from_le_bytes(dump[24..32].try_into().expect("8 bytes")) > 0);
        assert_eq!(u64::from_le_bytes(dump[32..40].try_into().expect("8 bytes")), 0);
    }

    /// `false`, `fp` and `no_fp` select no file at all — nothing is created.
    #[test]
    fn options_without_a_file_create_nothing() {
        for value in ["false", "fp", "no_fp"] {
            let dir = scratch(&format!("none_{value}"));
            let mut config = PerfMapConfig::new();
            config.set_directory(&dir);
            config.set_flag(value).expect("a grammar value");
            let perf = PerfMap::new(config);
            assert!(!perf.is_active(), "{value}");
            assert_eq!(perf.error(), None, "{value}");
            assert_eq!(perf.map_path(), None, "{value}");
            assert_eq!(perf.dump_path(), None, "{value}");
            let entries = std::fs::read_dir(&dir).expect("the scratch dir is readable");
            assert_eq!(entries.count(), 0, "`{value}` must write no file");
        }
    }

    /// Recording into a closed map is a typed error, never a silent no-op.
    #[test]
    fn recording_without_a_map_is_an_error() {
        let mut perf = PerfMap::new(PerfMapConfig::new());
        let error = perf.record(0x10, 0x2, "m:f/0").expect_err("no map");
        assert!(error.to_string().contains("perf map"), "{error}");
        assert_eq!(perf.entry_count(), 0);
    }

    /// An unopenable file is **recorded**, never raised and never fatal: the run continues
    /// exactly as the oracle's does (it logs and exits 0).
    #[test]
    fn an_unopenable_file_is_recorded_not_raised() {
        let missing = std::env::temp_dir()
            .join(format!("beam_rt_perf_map_missing_{}", std::process::id()))
            .join("no_such_subdirectory");
        let _ = std::fs::remove_dir_all(missing.parent().expect("a parent"));
        let mut config = PerfMapConfig::new();
        config.set_directory(&missing);
        config.set_flag("map").expect("map is a grammar value");
        let mut perf = PerfMap::new(config);
        assert!(!perf.is_active());
        assert_eq!(perf.map_path(), None);
        let reason = perf.error().expect("the failure is recorded");
        assert!(reason.starts_with("perf: failed to open "), "{reason}");
        assert!(reason.contains("no_such_subdirectory"), "{reason}");
        // The requested path is still answerable, and recording is still a typed error.
        assert_eq!(
            perf.config().map_path(std::process::id()),
            missing.join(format!("perf-{}.map", std::process::id()))
        );
        assert!(perf.record(0x10, 0x2, "m:f/0").is_err());
        assert_eq!(perf.entry_count(), 0);
    }
}
