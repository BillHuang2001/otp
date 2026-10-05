//! Runtime resolution and process observation.
//!
//! Two runtimes are observed and compared (contract A.6: `beam_rt` is a drop-in `erl`
//! for the expression cases — the *only* difference there is the program path):
//!
//! * the **oracle** — `$ERTS_ORACLE` if set (the path must exist), otherwise `erl`
//!   resolved by scanning `$PATH`. Nothing found ⇒ [`Error::Env`] (exit 2).
//! * the **Rust runtime** — `$BEAM_RT` if set (the path must exist, else [`Error::Env`]),
//!   otherwise the workspace-relative `target/{release,debug}/beam_rt`. Neither exists ⇒
//!   `Ok(None)`; the harness then reports the Rust side as *unsupported* instead of
//!   inventing a result.
//!
//! The harness never inspects runtime internals: it only spawns the program, feeds it
//! argv with `stdin` closed, and records the observed stdout/stderr/exit status. The two
//! runtimes receive **identical** argv for the expression corpus cases but **different**
//! argv for the module-run cases (the oracle's `-eval` versus the Rust `run` action), so
//! each [`Observation`] records the argv it was actually spawned with.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// The exit code the Rust runtime uses to report a **capability gap** (an operation
/// Milestone-1 does not implement) — distinct from `1` (a genuine runtime error) and `2`
/// (a usage error).
///
/// This is the documented external signal the harness keys off: an exit-`3` observation is
/// classified as [`SideResult::Unsupported`](crate::compare::SideResult), never as a
/// differing observable result, so an unimplemented feature is `unsupported`, not a
/// `mismatch`. The signal accompanies exactly one `beam_rt: unsupported: <reason>` stderr
/// line (see `../../crates/beam_rt/src/cli.rs`).
pub const UNSUPPORTED_EXIT_CODE: i32 = 3;

/// Which of the two runtimes an observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeId {
    /// The real system ERTS — the compatibility oracle.
    Oracle,
    /// The Rust runtime (`beam_rt`).
    Rust,
}

impl RuntimeId {
    /// Stable identifier used in reports (`"oracle"` / `"rust"`).
    pub fn id(&self) -> &'static str {
        match self {
            RuntimeId::Oracle => "oracle",
            RuntimeId::Rust => "rust",
        }
    }
}

/// How a runtime's program path was obtained (reported, so results are reproducible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeSource {
    /// An environment variable override, named with its `$` prefix.
    Env(&'static str),
    /// Resolved by scanning `$PATH`.
    Path,
    /// The documented workspace-relative default (`erts_rust/target/...`).
    WorkspaceDefault,
}

impl RuntimeSource {
    /// Stable identifier used in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeSource::Env(name) => name,
            RuntimeSource::Path => "$PATH",
            RuntimeSource::WorkspaceDefault => "workspace-default",
        }
    }
}

/// A resolved runtime: what to spawn and why that program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSpec {
    /// Which side of the comparison this is.
    pub id: RuntimeId,
    /// The executable to spawn.
    pub program: PathBuf,
    /// How `program` was resolved.
    pub source: RuntimeSource,
}

/// One observed run of a runtime: exactly what the harness fed in and saw come out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Which runtime was observed.
    pub runtime: RuntimeId,
    /// The program that was spawned.
    pub program: PathBuf,
    /// The argv the program was spawned with (per runtime — the expression cases share
    /// one argv, the module-run cases differ).
    pub argv: Vec<String>,
    /// Captured standard output (raw bytes — never re-encoded).
    pub stdout: Vec<u8>,
    /// Captured standard error (raw bytes).
    pub stderr: Vec<u8>,
    /// Process exit code, or `None` when the process was terminated by a signal.
    pub exit_code: Option<i32>,
}

/// Resolve the oracle runtime from the real environment.
pub fn resolve_oracle() -> Result<RuntimeSpec> {
    resolve_oracle_from(std::env::var_os("ERTS_ORACLE"), std::env::var_os("PATH"))
}

/// Resolve the oracle from explicit inputs (the injectable core of [`resolve_oracle`]).
///
/// * `erts_oracle` — the value of `$ERTS_ORACLE`, if set. It must name an existing
///   executable, otherwise this is a loud [`Error::Env`].
/// * `path_var` — the value of `$PATH` used to locate `erl` when `$ERTS_ORACLE` is unset.
pub fn resolve_oracle_from(
    erts_oracle: Option<OsString>,
    path_var: Option<OsString>,
) -> Result<RuntimeSpec> {
    if let Some(value) = erts_oracle {
        let program = PathBuf::from(&value);
        if !is_executable(&program) {
            return Err(Error::env(format!(
                "$ERTS_ORACLE is set to '{}', which is not an existing executable",
                program.display()
            )));
        }
        return Ok(RuntimeSpec {
            id: RuntimeId::Oracle,
            program,
            source: RuntimeSource::Env("$ERTS_ORACLE"),
        });
    }

    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for name in oracle_program_names() {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Ok(RuntimeSpec {
                        id: RuntimeId::Oracle,
                        program: candidate,
                        source: RuntimeSource::Path,
                    });
                }
            }
        }
    }

    Err(Error::env(
        "cannot locate the oracle runtime: $ERTS_ORACLE is unset and no 'erl' was found on $PATH \
         (set $ERTS_ORACLE to the oracle erl executable)",
    ))
}

/// Resolve the Rust runtime from the real environment.
///
/// `Ok(None)` means "there is no Rust runtime to observe" — the caller must report the
/// Rust side as *unsupported*, never as agreement.
pub fn resolve_rust() -> Result<Option<RuntimeSpec>> {
    resolve_rust_from(
        std::env::var_os("BEAM_RT"),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolve the Rust runtime from explicit inputs (the injectable core of [`resolve_rust`]).
///
/// * `beam_rt` — the value of `$BEAM_RT`, if set. It must name an existing file,
///   otherwise this is a loud [`Error::Env`].
/// * `manifest_dir` — the crate directory (`$CARGO_MANIFEST_DIR`); the workspace default
///   is `<manifest_dir>/../../target/{release,debug}/beam_rt`.
pub fn resolve_rust_from(
    beam_rt: Option<OsString>,
    manifest_dir: &Path,
) -> Result<Option<RuntimeSpec>> {
    if let Some(value) = beam_rt {
        let program = PathBuf::from(&value);
        if !program.is_file() {
            return Err(Error::env(format!(
                "$BEAM_RT is set to '{}', which is not an existing file",
                program.display()
            )));
        }
        return Ok(Some(RuntimeSpec {
            id: RuntimeId::Rust,
            program,
            source: RuntimeSource::Env("$BEAM_RT"),
        }));
    }

    let target = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(|workspace| workspace.join("target"))
        .ok_or_else(|| {
            Error::env(format!(
                "cannot derive the workspace target directory from CARGO_MANIFEST_DIR='{}'",
                manifest_dir.display()
            ))
        })?;

    for profile in ["release", "debug"] {
        let candidate = target.join(profile).join(rust_program_name());
        if candidate.is_file() {
            return Ok(Some(RuntimeSpec {
                id: RuntimeId::Rust,
                program: candidate,
                source: RuntimeSource::WorkspaceDefault,
            }));
        }
    }
    Ok(None)
}

/// Build the argv for an evaluation: `["-noshell", <extra…>, "-eval", <expr>]`.
///
/// The expression corpus cases hand this same argv to both runtimes; module-run cases
/// build their argv per side (see [`crate::corpus::ModuleRun`]).
pub fn eval_argv(expr: &str, extra: &[&str]) -> Vec<String> {
    let extra: Vec<String> = extra.iter().map(|arg| (*arg).to_string()).collect();
    eval_argv_owned(expr, &extra)
}

/// [`eval_argv`] for dynamically built extra arguments (e.g. repeated `-pa <dir>`).
pub fn eval_argv_owned(expr: &str, extra: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(extra.len() + 3);
    argv.push("-noshell".to_string());
    argv.extend(extra.iter().cloned());
    argv.push("-eval".to_string());
    argv.push(expr.to_string());
    argv
}

/// Spawn `spec.program` with `argv`, with `stdin` closed, and capture its output.
pub fn run(spec: &RuntimeSpec, argv: &[String]) -> Result<Observation> {
    let output = Command::new(&spec.program)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| {
            Error::io(
                format!(
                    "failed to execute the {} runtime '{}' with argv {:?}",
                    spec.id.id(),
                    spec.program.display(),
                    argv
                ),
                source,
            )
        })?;

    Ok(Observation {
        runtime: spec.id,
        program: spec.program.clone(),
        argv: argv.to_vec(),
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

/// The file names tried when scanning `$PATH` for the oracle.
fn oracle_program_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["erl.exe", "erl.cmd"]
    }
    #[cfg(not(windows))]
    {
        &["erl"]
    }
}

/// The file name of the Rust runtime binary inside `target/<profile>/`.
fn rust_program_name() -> &'static str {
    #[cfg(windows)]
    {
        "beam_rt.exe"
    }
    #[cfg(not(windows))]
    {
        "beam_rt"
    }
}

#[cfg(unix)]
pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::{
        eval_argv, eval_argv_owned, resolve_oracle_from, resolve_rust_from, run, RuntimeId,
        RuntimeSource, RuntimeSpec,
    };
    use crate::error::Error;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// A scratch directory that removes itself when dropped.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "differential-runtime-{}-{}-{:?}",
                std::process::id(),
                tag,
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch { dir }
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write scratch file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn executable(dir: &Scratch, relative: &str) -> PathBuf {
        let path = dir.file(relative);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }

    #[test]
    fn eval_argv_matches_the_drop_in_erl_shape() {
        assert_eq!(
            eval_argv("EXPR", &[]),
            vec!["-noshell", "-eval", "EXPR"],
            "the expression-case probe argv is the drop-in erl shape"
        );
        assert_eq!(
            eval_argv("EXPR", &["-differential_test_flag", "hello"]),
            vec![
                "-noshell",
                "-differential_test_flag",
                "hello",
                "-eval",
                "EXPR"
            ]
        );
        assert_eq!(
            eval_argv_owned("E", &["-pa".to_string(), "/tmp/ebin".to_string()]),
            vec!["-noshell", "-pa", "/tmp/ebin", "-eval", "E"]
        );
    }

    #[test]
    fn oracle_env_override_is_used_when_it_exists() {
        let dir = Scratch::new("oracle-env");
        let erl = executable(&dir, "bin/erl");
        let spec = resolve_oracle_from(Some(erl.clone().into_os_string()), None).expect("resolve");
        assert_eq!(spec.id, RuntimeId::Oracle);
        assert_eq!(spec.program, erl);
        assert_eq!(spec.source, RuntimeSource::Env("$ERTS_ORACLE"));
    }

    #[test]
    fn oracle_env_override_that_does_not_exist_is_a_loud_env_error() {
        let err = resolve_oracle_from(
            Some(OsString::from("/nonexistent/erl-differential-test")),
            None,
        )
        .expect_err("must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$ERTS_ORACLE"));
    }

    #[test]
    fn oracle_falls_back_to_scanning_path() {
        let dir = Scratch::new("oracle-path");
        let erl = executable(&dir, "erl");
        let other = dir.dir.join("nothing-here");
        let path = std::env::join_paths([other, dir.dir.clone()]).expect("join paths");
        let spec = resolve_oracle_from(None, Some(path)).expect("resolve");
        assert_eq!(spec.program, erl);
        assert_eq!(spec.source, RuntimeSource::Path);
    }

    #[test]
    fn oracle_without_env_or_path_is_a_loud_env_error() {
        let dir = Scratch::new("oracle-none");
        let path = std::env::join_paths([dir.dir.clone()]).expect("join paths");
        let err = resolve_oracle_from(None, Some(path))
            .expect_err("must not resolve a non-existing erl");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert!(err.to_string().contains("$ERTS_ORACLE is unset"));
    }

    #[test]
    fn rust_env_override_is_used_when_it_exists() {
        let dir = Scratch::new("rust-env");
        let beam_rt = dir.file("beam_rt");
        let spec = resolve_rust_from(Some(beam_rt.clone().into_os_string()), &dir.dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.id, RuntimeId::Rust);
        assert_eq!(spec.program, beam_rt);
        assert_eq!(spec.source, RuntimeSource::Env("$BEAM_RT"));
    }

    #[test]
    fn rust_env_override_that_does_not_exist_is_a_loud_env_error() {
        let dir = Scratch::new("rust-env-missing");
        let err = resolve_rust_from(Some(OsString::from("/nonexistent/beam_rt")), &dir.dir)
            .expect_err("must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$BEAM_RT"));
    }

    #[test]
    fn rust_defaults_to_the_workspace_target_and_is_absent_when_not_built() {
        let dir = Scratch::new("rust-default");
        let manifest_dir = dir.dir.join("erts_rust/tools/differential");
        std::fs::create_dir_all(&manifest_dir).expect("create manifest dir");
        assert_eq!(resolve_rust_from(None, &manifest_dir).expect("resolve"), None);

        let beam_rt = dir.file("erts_rust/target/debug/beam_rt");
        let spec = resolve_rust_from(None, &manifest_dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.program, beam_rt);
        assert_eq!(spec.source, RuntimeSource::WorkspaceDefault);

        // release wins over debug.
        let release = dir.file("erts_rust/target/release/beam_rt");
        let spec = resolve_rust_from(None, &manifest_dir)
            .expect("resolve")
            .expect("present");
        assert_eq!(spec.program, release);
    }

    #[test]
    fn run_captures_stdout_stderr_and_exit_status() {
        let dir = Scratch::new("run");
        let script = executable(&dir, "probe");
        std::fs::write(&script, b"#!/bin/sh\necho out\necho err 1>&2\nexit 7\n")
            .expect("write script");
        let spec = RuntimeSpec {
            id: RuntimeId::Rust,
            program: script.clone(),
            source: RuntimeSource::WorkspaceDefault,
        };
        let argv = vec!["-noshell".to_string(), "-eval".to_string()];
        let observation = run(&spec, &argv).expect("run");
        assert_eq!(observation.runtime, RuntimeId::Rust);
        assert_eq!(observation.program, script);
        assert_eq!(observation.argv, argv);
        assert_eq!(observation.stdout, b"out\n");
        assert_eq!(observation.stderr, b"err\n");
        assert_eq!(observation.exit_code, Some(7));
    }

    #[test]
    fn run_reports_a_missing_program_as_an_io_error() {
        let spec = RuntimeSpec {
            id: RuntimeId::Oracle,
            program: PathBuf::from("/nonexistent/erl-differential-test"),
            source: RuntimeSource::Path,
        };
        let err = run(&spec, &["-noshell".to_string()]).expect_err("must fail to spawn");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("oracle"));
    }

    #[test]
    fn default_resolvers_read_the_real_environment() {
        // Not asserting success: the point is that they must not panic and that the
        // workspace default is derived from this crate's own manifest directory.
        let _ = super::resolve_oracle();
        let _ = super::resolve_rust();
        assert!(Path::new(env!("CARGO_MANIFEST_DIR")).is_dir());
    }
}
