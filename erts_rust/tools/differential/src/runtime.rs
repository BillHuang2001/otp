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
//! The workspace default searches `release` **before** `debug` on purpose: it is what the
//! real CLI and the CI `gate.sh` want, so the fastest artifact present is measured. That
//! order is *wrong* under `cargo test`, where only the `debug` artifacts are (re)built — a
//! stale `target/release/beam_rt` could then shadow the fresh build. The integration tests
//! therefore inject `$BEAM_RT` (the fresh debug `beam_rt` sibling of the test binary; see
//! `../../tests/common/mod.rs`), so a stale release binary is never observed by a test.
//! This is purely a resolution *preference*: the order itself is never changed here.
//!
//! The harness never inspects runtime internals: it only spawns the program, feeds it
//! argv with `stdin` closed, and records the observed stdout/stderr/exit status. The two
//! runtimes receive **identical** argv for the expression corpus cases but **different**
//! argv for the module-run cases (the oracle's `-eval` versus the Rust `run` action), so
//! each [`Observation`] records the argv it was actually spawned with. An environment
//! case ([`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv),
//! [`CaseKind::ModuleEnv`](crate::corpus::CaseKind::ModuleEnv)) additionally spawns both
//! runtimes with an explicit environment ([`run_with_env`]), so their `ERL_*FLAGS` merge —
//! and, for a module-run case, the flag's *application* to the code path — is what is
//! compared.

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
///
/// The `release` profile is searched before `debug` deliberately: the real CLI and
/// `gate.sh` want the fastest artifact available. Integration tests must **not** rely on
/// this default — they inject `$BEAM_RT` pointing at the freshly built debug `beam_rt`
/// (see `../../tests/common/mod.rs`), so a stale `release` binary can never shadow a fresh
/// `debug` build. The preference order is intentionally left unchanged.
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

/// How many times a child spawn is attempted before a transient `ETXTBSY` is surfaced.
///
/// The race is short-lived (it lasts only until the thread that is writing the script
/// closes its descriptor), so a handful of attempts suffice. The bound matters: a
/// *permanently* write-held file (or any other failure) is returned, loudly, after this
/// many attempts — the retry can never spin forever or swallow an error.
const SPAWN_ATTEMPTS: u32 = 6;

/// The first backoff before retrying a transient `ETXTBSY` spawn.
const SPAWN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

/// The cap the `ETXTBSY` backoff grows to (it doubles after each attempt).
///
/// With [`SPAWN_ATTEMPTS`] `= 6` and a 5 ms first backoff, the total bounded window is
/// `5 + 10 + 20 + 40 + 80 = 155 ms`: long enough that a writer held open by a *sibling*
/// thread under heavy parallel load (a testing/pre-building process) has released it,
/// short enough that a genuine, permanent failure is still surfaced promptly.
const SPAWN_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(80);

/// Spawn `cmd` and collect its output, retrying **only** the transient `ETXTBSY` race.
///
/// Both spawn sites in this crate legitimately execute a *freshly written* `#!/bin/sh`
/// script: `$ERTS_ORACLE`/`$BEAM_RT` may name a wrapper script, and `module_build`-style
/// stub compilers are scripts too. When one thread `fork()`s (via `Command::output`) while
/// another thread still holds an open descriptor **for writing** that script, the child
/// transiently inherits the descriptor; if the other thread then `exec`s the script the
/// kernel refuses with `ETXTBSY` ("Text file busy", errno 26). This is a pure scheduling
/// artifact — the file is fully written and executable — so retrying after a short pause
/// yields exactly the result the first attempt would have produced had the other thread
/// closed its descriptor first. It is therefore *not* error swallowing: every non-`ETXTBSY`
/// error, and the `ETXTBSY` that survives all [`SPAWN_ATTEMPTS`] attempts, is returned
/// unchanged for the caller to surface loudly.
///
/// The backoff **doubles** after each attempt, capped at [`SPAWN_BACKOFF_MAX`], so the
/// total wait (155 ms) comfortably outlasts a writer held open by a concurrent `fork()`
/// under parallel test load while staying bounded.
pub(crate) fn output_with_retry(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    let mut attempt = 1;
    let mut backoff = SPAWN_BACKOFF;
    loop {
        match cmd.output() {
            Ok(output) => return Ok(output),
            Err(err) if attempt < SPAWN_ATTEMPTS && is_transient_text_file_busy(&err) => {
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2).min(SPAWN_BACKOFF_MAX);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Whether `err` is the transient `ETXTBSY` ("Text file busy") fork/exec race described on
/// [`output_with_retry`].
fn is_transient_text_file_busy(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::ExecutableFileBusy {
        return true;
    }
    // Belt and suspenders: accept the raw errno too, in case a platform/libc mapping does
    // not surface `ETXTBSY` as `ErrorKind::ExecutableFileBusy`.
    #[cfg(unix)]
    {
        const ETXTBSY: i32 = 26;
        if err.raw_os_error() == Some(ETXTBSY) {
            return true;
        }
    }
    false
}

/// Spawn `spec.program` with `argv` and the inherited environment, with `stdin` closed,
/// and capture its output.
pub fn run(spec: &RuntimeSpec, argv: &[String]) -> Result<Observation> {
    run_with_env(spec, argv, &[])
}

/// [`run`] with an explicit child **environment** (contract A.6).
///
/// Each `(name, value)` is set on the child *in addition to* the inherited environment —
/// the harness never clears it, so the runtime's own resolution (`$PATH`, `$TMPDIR`, …)
/// stays undisturbed. This is how an `ERL_*FLAGS` corpus case
/// ([`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv),
/// [`CaseKind::ModuleEnv`](crate::corpus::CaseKind::ModuleEnv)) pins the environment-variable
/// merge — `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]`
/// — against the oracle: both runtimes are spawned with the identical argv **and** the
/// identical environment, so only their behavior can differ.
///
/// The pairs are **owned** (`String`) because a module-run case's value may embed the
/// compiled module's temp directory, which is only known at run time
/// ([`crate::corpus::expand_env`]); [`own_env`] converts a case's `'static` slice.
pub fn run_with_env(
    spec: &RuntimeSpec,
    argv: &[String],
    env: &[(String, String)],
) -> Result<Observation> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        cmd.env(name, value);
    }
    let output = output_with_retry(&mut cmd).map_err(|source| {
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

/// Own the `(name, value)` pairs of a case's `'static` environment slice, so it can be
/// handed to [`run_with_env`].
///
/// A [`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv) or
/// [`CaseKind::ModuleEnv`](crate::corpus::CaseKind::ModuleEnv) case stores its environment as
/// `&'static [(&'static str, &'static str)]` (so a [`crate::corpus::Case`] stays `Copy`);
/// [`run_with_env`] takes owned pairs. A `ModuleEnv` value that embeds the compiled module's
/// temp dir goes through [`crate::corpus::expand_env`] instead, which substitutes the
/// directory at the same time.
pub fn own_env(env: &[(&str, &str)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
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
        eval_argv, eval_argv_owned, is_transient_text_file_busy, output_with_retry, own_env,
        resolve_oracle_from, resolve_rust_from, run, run_with_env, RuntimeId, RuntimeSource,
        RuntimeSpec,
    };
    use crate::error::Error;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;

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
    fn run_with_env_sets_the_child_environment_and_keeps_the_inherited_one() {
        // A short-lived `sh` is the portable way to observe the child environment: it
        // prints the variable the harness set, and an empty line when it is unset.
        #[cfg(unix)]
        {
            let spec = RuntimeSpec {
                id: RuntimeId::Rust,
                program: PathBuf::from("sh"),
                source: RuntimeSource::Path,
            };
            let argv = vec![
                "-c".to_string(),
                String::from("printf %s \"$DIFFERENTIAL_RUN_ENV_PROBE\""),
            ];
            let with_env = run_with_env(
                &spec,
                &argv,
                &[("DIFFERENTIAL_RUN_ENV_PROBE".to_string(), "hello-env".to_string())],
            )
            .expect("spawn sh with the probe variable");
            assert_eq!(with_env.exit_code, Some(0));
            assert_eq!(with_env.stdout, b"hello-env");
            assert_eq!(
                with_env.argv, argv,
                "the observed argv is recorded verbatim"
            );

            // `run` is `run_with_env(…, &[])`: the variable is unset, so the shell prints
            // nothing — the inherited environment is otherwise untouched.
            let without_env = run(&spec, &argv).expect("spawn sh without the probe variable");
            assert_eq!(without_env.stdout, b"");
        }
    }

    #[test]
    fn own_env_copies_the_pairs_verbatim() {
        assert_eq!(
            own_env(&[("ERL_AFLAGS", "-pa {dir}"), ("ERL_ZFLAGS", "+S 2")]),
            vec![
                ("ERL_AFLAGS".to_string(), "-pa {dir}".to_string()),
                ("ERL_ZFLAGS".to_string(), "+S 2".to_string()),
            ],
            "the placeholder is left for the harness to expand"
        );
        assert!(own_env(&[]).is_empty());
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
    fn output_with_retry_passes_a_successful_run_through_unchanged() {
        let dir = Scratch::new("retry-ok");
        let script = executable(&dir, "probe");
        std::fs::write(&script, b"#!/bin/sh\necho out\nexit 0\n").expect("write script");
        let mut cmd = Command::new(&script);
        let output = output_with_retry(&mut cmd).expect("spawn");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out\n");
    }

    #[test]
    fn output_with_retry_surfaces_a_non_transient_error_loudly() {
        // A missing executable is not an ETXTBSY race: it must come straight back, so the
        // bounded retry can never mask (or delay) a genuine spawn failure.
        let mut cmd = Command::new("/nonexistent/erl-differential-retry-test");
        let err = output_with_retry(&mut cmd).expect_err("must fail to spawn");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err:?}");
    }

    #[test]
    fn is_transient_text_file_busy_matches_only_etxtbsy() {
        assert!(is_transient_text_file_busy(&std::io::Error::from(
            std::io::ErrorKind::ExecutableFileBusy
        )));
        assert!(is_transient_text_file_busy(
            &std::io::Error::from_raw_os_error(26)
        ));
        assert!(!is_transient_text_file_busy(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_transient_text_file_busy(
            &std::io::Error::from_raw_os_error(13)
        ));
    }

    /// Deterministically exercises the `ETXTBSY` path the retry exists for.
    ///
    /// Linux refuses to `exec` a file that **any** process (including this one) holds open
    /// for writing, with `ETXTBSY` — exactly the transient error a concurrent `fork()` can
    /// cause for a freshly written script. Holding the descriptor open therefore makes
    /// *every* attempt fail, which pins down two behaviors a timing-dependent test could
    /// not: the retry is **bounded** (it gives up and returns the error instead of spinning
    /// or swallowing it), and once the writer is gone the very same command runs.
    #[cfg(unix)]
    #[test]
    fn output_with_retry_is_bounded_on_etxtbsy_and_surfaces_it() {
        let dir = Scratch::new("retry-etxtbsy");
        let script = executable(&dir, "held-open");
        std::fs::write(&script, b"#!/bin/sh\necho out\nexit 0\n").expect("write script");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&script)
            .expect("open the script for writing");

        let mut cmd = Command::new(&script);
        let err = output_with_retry(&mut cmd).expect_err("a write-open file cannot be exec'd");
        assert!(
            is_transient_text_file_busy(&err),
            "expected ETXTBSY, got {err:?}"
        );

        drop(held);
        let output = output_with_retry(&mut cmd).expect("spawn once the writer closed");
        assert_eq!(output.stdout, b"out\n");
    }

    /// The retry must actually **recover** when the transient writer goes away.
    ///
    /// [`output_with_retry_is_bounded_on_etxtbsy_and_surfaces_it`] pins the "gives up
    /// loudly" half; this pins the "succeeds once the race resolves" half, which is what
    /// makes the suite deterministic under parallel load. The file is held open for
    /// writing by *another thread* (mirroring a concurrent `fork()` inheriting a writer's
    /// descriptor) and released while the spawn is already retrying, so on Linux the first
    /// attempts deterministically fail with `ETXTBSY` and a later attempt succeeds.
    #[cfg(unix)]
    #[test]
    fn output_with_retry_recovers_once_the_transient_writer_closes() {
        let dir = Scratch::new("retry-recover");
        let script = executable(&dir, "settle");
        std::fs::write(&script, b"#!/bin/sh\necho out\nexit 0\n").expect("write script");

        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&script)
            .expect("open the script for writing");
        let release = std::thread::spawn(move || {
            // Released well within the 155 ms bounded retry window.
            std::thread::sleep(std::time::Duration::from_millis(25));
            drop(held);
        });

        let mut cmd = Command::new(&script);
        let output = output_with_retry(&mut cmd)
            .expect("the bounded retry must recover once the writer closes");
        assert_eq!(output.stdout, b"out\n");
        release.join().expect("writer thread");
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
