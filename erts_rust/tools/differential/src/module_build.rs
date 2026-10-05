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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::runtime::{is_executable, output_with_retry};

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

    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for name in erlc_program_names() {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Ok(Some(candidate));
                }
            }
        }
    }
    Ok(None)
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
pub fn compile(erlc: &Path, source: &str, module: &str) -> Result<CompiledModule> {
    let dir = unique_temp_dir(module);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|source| {
        Error::io(
            format!("failed to create the module temp dir '{}'", dir.display()),
            source,
        )
    })?;

    let source_path = dir.join(format!("{module}.erl"));
    if let Err(source) = std::fs::write(&source_path, source) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Error::io(
            format!("failed to write the module source '{}'", source_path.display()),
            source,
        ));
    }

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
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// An executable scratch file (a `#!/bin/sh` stub), removed with its directory.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "differential-module-build-{}-{}-{:?}",
                std::process::id(),
                tag,
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch { dir }
        }

        fn executable(&self, name: &str, body: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, body).expect("write script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path).expect("stat").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).expect("chmod");
            }
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

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
    fn resolve_erlc_reads_the_real_environment_without_panicking() {
        let _ = resolve_erlc();
    }
}
