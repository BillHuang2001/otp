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

//! Module-run support: compile an embedded Erlang module with the system `erlc`.
//!
//! The module-run corpus cases ([`crate::corpus::CaseKind::Module`]) compare a *compiled*
//! `.beam` executed by both runtimes. The module source is embedded in this crate; this
//! module writes it to a unique temporary directory and compiles it with the system
//! `erlc`, returning a [`CompiledModule`] that removes the directory when dropped.
//!
//! `erlc` is resolved from `$ERLC` if set (which must name an existing executable, else a
//! loud [`Error::Env`], mirroring `$ERTS_ORACLE`/`$BEAM_RT`), otherwise by scanning
//! `$PATH`. When no `erlc` exists, [`resolve_erlc`] returns `Ok(None)`: the harness then
//! **skips** the module-run case loudly (never a silent pass). A present `erlc` that fails
//! to compile is a genuine tool failure (a loud error), not a skip.
//!
//! # C/NIF fixtures (contract A.3)
//!
//! A module-run case may additionally carry a [`NifFixture`] — the C (or plain-text) artifacts
//! that must sit **next to the compiled `.beam`**. The fixture's host-tool resolution, the
//! `cc` invocation that builds it, and its byte cache live in the sibling `nif_build` module;
//! this module re-exports that surface so a caller keeps treating the whole "build a
//! module-run case" facility as [`crate::module_build`] ([`build`], [`Build`], [`NifFixture`]).
//!
//! # Compile cache (iteration speed)
//!
//! The corpus compiles the *same* handful of embedded sources once per module-run case
//! (~50 cases, 2 distinct sources), so [`compile`] memoizes the `erlc` output in a
//! **process-local cache** keyed by `(erlc, module, source)`. A cache hit *materializes* a
//! fresh per-case temp dir from the cached `.beam` bytes without spawning `erlc` again —
//! the bytes, the argv and therefore every observable result are identical, only the
//! redundant compiler process disappears. Each call still owns its own directory, removed
//! by [`CompiledModule`]'s `Drop` exactly as before.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::error::{Error, Result};
use crate::runtime::{is_executable, output_with_retry};

pub use crate::nif_build::{
    build, resolve_cc, resolve_cc_from, resolve_erl_include, Build, NifFixture, MISSING_CC_REASON,
    MISSING_ERL_NIF_REASON,
};

/// The reason recorded when no `erlc` could be found (the harness skips the case with it).
pub const MISSING_ERLC_REASON: &str = "no erlc compiler found: $ERLC is unset and no 'erlc' \
     was found on $PATH (the module-run cases need the system compiler)";

/// Resolve `erlc` from the real environment.
///
/// `Ok(None)` means "no compiler to compile a module with" — the caller must **skip** the
/// module-run case, never report agreement.
pub fn resolve_erlc() -> Result<Option<PathBuf>> {
    resolve_erlc_from(std::env::var_os("ERLC"), std::env::var_os("PATH"))
}

/// Resolve `erlc` from explicit inputs (the injectable core of [`resolve_erlc`]).
///
/// * `erlc` — the value of `$ERLC`, if set. It must name an existing executable,
///   otherwise this is a loud [`Error::Env`].
/// * `path_var` — the value of `$PATH` used to locate `erlc` when `$ERLC` is unset.
pub fn resolve_erlc_from(erlc: Option<OsString>, path_var: Option<OsString>) -> Result<Option<PathBuf>> {
    if let Some(value) = erlc {
        let program = PathBuf::from(&value);
        if !is_executable(&program) {
            return Err(Error::env(format!(
                "$ERLC is set to '{}', which is not an existing executable",
                program.display()
            )));
        }
        return Ok(Some(program));
    }

    Ok(path_var
        .as_deref()
        .and_then(|path_var| find_on_path(erlc_program_names(), path_var)))
}

/// The first executable named any of `names` in the `$PATH` value `path_var`.
///
/// Shared by the `erlc`/`cc` resolvers (`cc` lives in [`crate::nif_build`]).
pub(crate) fn find_on_path(names: &[&str], path_var: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// A module compiled into a temporary directory, removed when dropped.
#[derive(Debug)]
pub struct CompiledModule {
    dir: PathBuf,
    beam: PathBuf,
}

impl CompiledModule {
    /// The temporary directory holding the compiled output (passed to the runtimes as
    /// `-pa <dir>` and `<dir>/<module>.beam`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The compiled `<dir>/<module>.beam`.
    pub fn beam(&self) -> &Path {
        &self.beam
    }
}

impl Drop for CompiledModule {
    fn drop(&mut self) {
        // Best-effort cleanup: a failure to remove the temp dir must not mask the case's
        // result (the directory is unique and under the system temp dir).
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Compile `source` (declaring a module named `module`) with `erlc` into a fresh temp dir.
///
/// A missing `erlc` binary or a compile failure is a loud [`Error`] (the caller decides
/// whether to run the case). On any failure the temp directory is removed before
/// returning.
///
/// The `erlc` invocation is memoized process-locally by `(erlc, module, source)`: a repeat
/// compile of the same source reuses the cached `.beam` bytes (see the module docs). The
/// returned [`CompiledModule`] always names its **own** fresh temp dir and removes it on
/// drop, whether it came from `erlc` or from the cache.
pub fn compile(erlc: &Path, source: &str, module: &str) -> Result<CompiledModule> {
    let key = CacheKey {
        erlc: erlc.to_path_buf(),
        module: module.to_string(),
        source: source.to_string(),
    };

    if let Some(beam) = cached_beam(&key) {
        return materialize(module, source, &beam);
    }

    let compiled = compile_with_erlc(erlc, source, module)?;
    // Cache the artifact so the next case with the same source skips the compiler. A beam
    // we cannot read afterwards cannot be run by either runtime either, so surface it loudly.
    let beam = std::fs::read(compiled.beam()).map_err(|source| {
        Error::io(
            format!(
                "failed to read the compiled beam '{}'",
                compiled.beam().display()
            ),
            source,
        )
    })?;
    store_beam(key, beam);
    Ok(compiled)
}

/// The uncached compile: run the real `erlc` into a fresh temp dir.
fn compile_with_erlc(erlc: &Path, source: &str, module: &str) -> Result<CompiledModule> {
    let dir = fresh_temp_dir(module)?;
    let source_path = dir.join(format!("{module}.erl"));
    write_into_temp(&dir, &source_path, source.as_bytes())?;

    // `output_with_retry` absorbs the transient `ETXTBSY` fork/exec race: `erlc` may be a
    // freshly written wrapper script (e.g. a test stub), so a concurrent `fork()` in another
    // test thread can transiently make `exec` fail with "Text file busy".
    let mut cmd = Command::new(erlc);
    cmd.arg("-o")
        .arg(&dir)
        .arg(&source_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = output_with_retry(&mut cmd).map_err(|source| {
        Error::io(
            format!("failed to execute erlc '{}'", erlc.display()),
            source,
        )
    })?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Error::io(
            format!(
                "erlc failed to compile '{}' (exit {:?}): {}",
                source_path.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            std::io::Error::new(std::io::ErrorKind::Other, "erlc reported a compile failure"),
        ));
    }

    let beam = dir.join(format!("{module}.beam"));
    if !beam.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Error::io(
            format!(
                "erlc produced no '{}' for module {module}",
                beam.display()
            ),
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing .beam output"),
        ));
    }

    Ok(CompiledModule { dir, beam })
}

/// A cache hit: write the *cached* `beam` bytes (plus the source, matching an `erlc`-built
/// directory) into a fresh temp dir without spawning `erlc`.
fn materialize(module: &str, source: &str, beam: &[u8]) -> Result<CompiledModule> {
    let dir = fresh_temp_dir(module)?;
    let source_path = dir.join(format!("{module}.erl"));
    write_into_temp(&dir, &source_path, source.as_bytes())?;
    let beam_path = dir.join(format!("{module}.beam"));
    write_into_temp(&dir, &beam_path, beam)?;
    Ok(CompiledModule {
        dir,
        beam: beam_path,
    })
}

/// Create a fresh, empty temp dir for `module` (removing a stale one of the same name left
/// by an earlier crashed run).
fn fresh_temp_dir(module: &str) -> Result<PathBuf> {
    let dir = unique_temp_dir(module);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|source| {
        Error::io(
            format!("failed to create the module temp dir '{}'", dir.display()),
            source,
        )
    })?;
    Ok(dir)
}

/// Write `bytes` to `path`; on failure remove the whole temp `dir` and fail loudly.
///
/// Shared with [`crate::nif_build`], which writes a case's C/NIF fixture into the **same**
/// temp dir.
pub(crate) fn write_into_temp(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    if let Err(source) = std::fs::write(path, bytes) {
        let _ = std::fs::remove_dir_all(dir);
        return Err(Error::io(
            format!("failed to write '{}'", path.display()),
            source,
        ));
    }
    Ok(())
}

/// The identity of a compiled artifact: everything that determines its `.beam` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    erlc: PathBuf,
    module: String,
    source: String,
}

/// The process-local compile cache: `(erlc, module, source) -> compiled `.beam` bytes.
///
/// It only ever short-circuits a redundant `erlc` spawn; the artifact handed to a case is
/// still written into that case's own temp dir, so the cache cannot change what is observed.
fn compile_cache() -> &'static Mutex<HashMap<CacheKey, Vec<u8>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cache lock, recovering from a poisoned lock (only `HashMap` reads/inserts happen
/// under it, so a recovered map is always consistent).
///
/// Shared with [`crate::nif_build`]'s `.so` cache.
pub(crate) fn cache_lock<K: Eq + Hash>(
    cache: &'static Mutex<HashMap<K, Vec<u8>>>,
) -> MutexGuard<'static, HashMap<K, Vec<u8>>> {
    cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A copy of the cached `.beam` bytes for `key`, or `None` on a miss.
fn cached_beam(key: &CacheKey) -> Option<Vec<u8>> {
    cache_lock(compile_cache()).get(key).cloned()
}

/// Record the compiled `.beam` bytes for `key`.
fn store_beam(key: CacheKey, beam: Vec<u8>) {
    cache_lock(compile_cache()).insert(key, beam);
}

/// A unique temporary directory name for one compiled module.
fn unique_temp_dir(module: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "differential-module-{}-{}-{}",
        std::process::id(),
        n,
        module
    ))
}

/// The file names tried when scanning `$PATH` for `erlc`.
fn erlc_program_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["erlc.exe", "erlc.cmd"]
    }
    #[cfg(not(windows))]
    {
        &["erlc"]
    }
}

#[cfg(test)]
mod tests {
    use super::{compile, resolve_erlc, resolve_erlc_from};
    use crate::error::Error;
    use crate::test_support::{counting_compiler, invocations, Scratch};
    use std::ffi::OsString;

    #[test]
    fn erlc_env_override_must_exist() {
        let err = resolve_erlc_from(Some(OsString::from("/nonexistent/erlc-differential")), None)
            .expect_err("a nonexistent $ERLC must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$ERLC"));
    }

    #[test]
    fn erlc_env_override_is_used_when_it_exists() {
        let scratch = Scratch::new("env");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let resolved = resolve_erlc_from(Some(erlc.clone().into_os_string()), None)
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved, erlc);
    }

    #[test]
    fn erlc_is_found_by_scanning_path() {
        let scratch = Scratch::new("path");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let other = scratch.dir.join("nothing-here");
        let path = std::env::join_paths([other, scratch.dir.clone()]).expect("join paths");
        let resolved = resolve_erlc_from(None, Some(path))
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved, erlc);
    }

    #[test]
    fn a_missing_erlc_is_ok_none() {
        let scratch = Scratch::new("absent");
        let path = std::env::join_paths([scratch.dir.clone()]).expect("join paths");
        assert_eq!(resolve_erlc_from(None, Some(path)).expect("resolve"), None);
    }

    #[test]
    fn a_failing_compiler_is_a_loud_error() {
        let scratch = Scratch::new("fail");
        let erlc = scratch.executable("erlc", "#!/bin/sh\necho 'boom' 1>&2\nexit 1\n");
        let err = compile(&erlc, "-module(x).\n", "x").expect_err("a compile failure is an error");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn a_compiler_that_writes_nothing_is_a_loud_error() {
        let scratch = Scratch::new("nooutput");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let err = compile(&erlc, "-module(x).\n", "x").expect_err("no .beam is an error");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("no"), "{err}");
    }

    /// The `erlc` spawn goes through the shared `ETXTBSY` retry, and that retry is bounded.
    ///
    /// Holding the stub compiler open for writing makes Linux refuse every `exec` with
    /// `ETXTBSY`, so `compile` must return the **loud spawn error** once the attempts are
    /// exhausted — not a bogus "no `.beam`" compile result, and not an endless retry.
    #[cfg(unix)]
    #[test]
    fn a_compiler_that_cannot_be_exec_yet_is_a_loud_error_not_a_hang() {
        let scratch = Scratch::new("etxtbsy");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&erlc)
            .expect("open the stub compiler for writing");

        // A permanently held writer must surface loudly, not hang or look like a compile
        // failure — the retry is bounded.
        let err = compile(&erlc, "-module(x).\n", "x")
            .expect_err("a write-open file cannot be exec'd");
        match err {
            Error::Io { ref source, .. } => {
                assert_eq!(source.raw_os_error(), Some(26), "{source:?}")
            }
            other => panic!("expected a loud spawn error, got {other:?}"),
        }
        assert!(err.to_string().contains("failed to execute erlc"), "{err}");

        drop(held);
    }

    #[test]
    fn a_program_that_fabricates_the_beam_is_accepted_and_cleaned_up() {
        // A stub compiler that just creates the expected output file: exercises the
        // success path (and the temp-dir cleanup on drop) without needing a real erlc.
        let scratch = Scratch::new("fake");
        let erlc = scratch.executable(
            "erlc",
            "#!/bin/sh\nout=\nsrc=\nwhile [ $# -gt 0 ]; do case $1 in -o) shift; out=$1;; *) src=$1;; esac; shift; done\n\
             base=$(basename \"$src\" .erl)\nprintf 'BEAM' > \"$out/$base.beam\"\n",
        );
        let compiled = compile(&erlc, "-module(x).\n", "x").expect("compile");
        assert!(compiled.beam().is_file(), "the .beam exists");
        assert_eq!(std::fs::read(compiled.beam()).expect("read beam"), b"BEAM");
        let dir = compiled.dir().to_path_buf();
        assert!(dir.is_dir());
        drop(compiled);
        assert!(!dir.exists(), "the temp dir is removed on drop");
    }

    #[test]
    fn a_repeated_compile_reuses_the_cached_beam_without_reinvoking_erlc() {
        let scratch = Scratch::new("cache");
        let counter = scratch.dir.join("invocations");
        let erlc = counting_compiler(&scratch, &counter);
        let source = "-module(cached_probe).\n";

        let first = compile(&erlc, source, "cached_probe").expect("first compile");
        assert_eq!(invocations(&counter), 1, "the first compile runs erlc");
        let first_dir = first.dir().to_path_buf();
        let first_beam = std::fs::read(first.beam()).expect("read the beam");

        // The same (erlc, module, source) triple is a cache hit: no second erlc process,
        // but the case still gets its own freshly materialized directory.
        let second = compile(&erlc, source, "cached_probe").expect("second compile");
        assert_eq!(
            invocations(&counter),
            1,
            "a repeated compile must not spawn erlc again"
        );
        assert_eq!(
            std::fs::read(second.beam()).expect("read the beam"),
            first_beam,
            "the cached beam bytes are identical"
        );
        assert_ne!(first_dir, second.dir(), "each case keeps its own temp dir");
        assert!(
            second.dir().join("cached_probe.erl").is_file(),
            "a materialized dir mirrors an erlc-built one (source + beam)"
        );

        // The key covers the source: a different source is a genuine cache miss.
        compile(&erlc, "-module(cached_probe).\n%% v2\n", "cached_probe").expect("third compile");
        assert_eq!(invocations(&counter), 2, "a different source recompiles");

        // Every materialized dir is cleaned up on drop, exactly like an erlc-built one.
        let second_dir = second.dir().to_path_buf();
        drop(first);
        drop(second);
        assert!(!first_dir.exists(), "the first temp dir is removed on drop");
        assert!(
            !second_dir.exists(),
            "the cached temp dir is removed on drop"
        );
    }

    #[test]
    fn concurrent_compiles_of_the_same_source_all_yield_a_usable_beam() {
        // The cache is shared, so it must be safe to reach from several threads at once:
        // every caller gets a valid, private temp dir and erlc never runs more than once
        // per thread.
        let scratch = Scratch::new("cache-threads");
        let counter = scratch.dir.join("invocations");
        let erlc = counting_compiler(&scratch, &counter);
        let source = "-module(cached_threads).\n";

        let results: Vec<_> = std::thread::scope(|scope| {
            (0..4)
                .map(|_| {
                    let erlc = erlc.clone();
                    let counter = counter.clone();
                    scope.spawn(move || {
                        let compiled = compile(&erlc, source, "cached_threads").expect("compile");
                        assert!(compiled.beam().is_file());
                        assert_eq!(std::fs::read(compiled.beam()).expect("read beam"), b"BEAM");
                        drop(compiled);
                        invocations(&counter)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        assert!(
            results.iter().all(|&n| (1usize..=4).contains(&n)),
            "erlc ran once per thread at most: {results:?}"
        );
    }

    #[test]
    fn resolve_erlc_reads_the_real_environment_without_panicking() {
        let _ = resolve_erlc();
    }
}
