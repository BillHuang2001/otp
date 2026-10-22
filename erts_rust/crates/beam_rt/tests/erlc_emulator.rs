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

//! Contract A.9 (build/bootstrapping), the **`erlc` route**: `erlc` is a shell script that
//! boots `$ERLC_EMULATOR` and runs the *whole* compiler front end (`erl_compile`) inside it,
//! so pointing `ERLC_EMULATOR` at this runtime is the smallest complete vertical slice of
//! "the Rust runtime runs the real toolchain".
//!
//! # The route
//!
//! `erlc` execs
//!
//! ```text
//! $ERLC_EMULATOR +S1 +sbtu +A0 +P 65536 +Q 1024 -noinput -mode minimal \
//!   -boot no_dot_erlang -s erl_compile compile_cmdline -extra -o DIR SRC…
//! ```
//!
//! (captured from this checkout with a stand-in `$ERLC_EMULATOR`) — the attached `+`-flag
//! spellings [`tests/cli_compact_flags.rs`](../cli_compact_flags.rs) pins, then a `-s` start
//! instruction, then the front end's own options as **plain arguments** behind `-extra`.
//! So this suite is the capstone over both A.6 changes: a regression in either the compact
//! `+`-flag reader or the `-extra` plain-argument split kills the route before it compiles
//! anything.
//!
//! # What the rows assert
//!
//! * **(a) the plain route** — `erlc -o A ecap.erl` through this runtime exits **0**, writes
//!   **nothing** to stdout or stderr, produces `ecap.beam`, and those bytes are identical to
//!   the artifact the **system** `erlc` produces for the same source in a second directory.
//! * **(b) the option-carrying route** — the same for `erlc +deterministic -I INC -o A
//!   finfo.erl`, whose source uses both `-include_lib("kernel/include/file.hrl")` (the
//!   compiler's own `code:lib_dir/2` probe) and `-include("hdrs.hrl")` (the `-I` path).
//! * **(c) the artifacts are real `.beam`s** — each one is loaded under the **oracle** and
//!   its export is printed: `ecap:double(21)` ⇒ `42`, `finfo:greet()` ⇒ `hello`. Byte
//!   equality alone would not prove the container is loadable; this does.
//! * **(d) a diagnostic-producing source compiles exactly as under the system `erlc`** — a
//!   source that makes the compiler emit a **warning** exits **0**, writes **nothing** to
//!   stdout, its stderr is **byte-identical** to the system `erlc`'s (the 90-byte
//!   [`WARN_DIAGNOSTIC`]), and `warn.beam` **is** emitted and byte-identical to the system
//!   compiler's artifact; a source with **compile errors** exits **1**, writes nothing to
//!   stdout, its stderr is byte-identical to the system `erlc`'s (the 184-byte
//!   [`BERR_DIAGNOSTIC`]), and **no** artifact is left behind on either side.
//!
//! # Harness
//!
//! `erlc` boots a whole node before it compiles, so every child here gets
//! [`ERLC_TIMEOUT`] (well above the shared [`support::PROBE_TIMEOUT`]) and is killed and
//! reaped by the file-local bounded runner ([`run_for`]) if it outlives it. A child never
//! inherits an `ERLC_EMULATOR`, an `ERL_*FLAGS` variable or a scheduling setting from the
//! host: the emulator is set explicitly per row and the reference row **removes** it. A
//! missing `erlc` is a **loud skip** ([`support::skip_loudly`]), never a silent pass.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// Fixtures (source only — compiled by whichever `erlc` the row drives)
// ---------------------------------------------------------------------------------------

/// A warning-free module with one export: `ecap:double(21)` is `42`.
const ECAP_ERL: &str = "\
-module(ecap).
-export([double/1]).
double(X) -> X * 2.
";

/// The include file `finfo.erl` pulls in through `-I`.
const HDRS_HRL: &str = "-define(GREETING, hello).\n";

/// A module that uses **both** include forms: `-include_lib/1` (resolved through the
/// compiler's own `code:lib_dir/2`) and `-include/1` (resolved through `erlc`'s `-I`).
///
/// Both functions are **exported** so `finfo.erl` yields a clean route that emits **no**
/// diagnostic at all — the option-carrying row asserts an empty stderr, so an unexported
/// function (which the compiler warns about) is avoided rather than tolerated.
const FINFO_ERL: &str = "\
-module(finfo).
-export([greet/0, size/0]).
-include_lib(\"kernel/include/file.hrl\").
-include(\"hdrs.hrl\").
greet() -> ?GREETING.
size() -> #file_info.size.
";

/// A module whose only diagnostic is a **warning** (`X` is unused): the compiler has a
/// complete artifact to write and one warning to report.
const WARN_ERL: &str = "\
-module(warn).
-export([f/0]).
f() -> X = 1, ok.
";

/// A module with real **compile errors** (an unbound variable and an undefined function):
/// the compiler has no artifact to write and two errors to report.
const BERR_ERL: &str = "\
-module(berr).
-export([f/0]).
f() -> unused(X), ok.
";

/// The compiler's own diagnostic for [`WARN_ERL`], byte for byte, cross-checked against the
/// system `erlc`'s: one unused-variable warning with its source excerpt (a caret line under
/// the offending column) and a blank line after it. The diagnostic names the file by its
/// **basename** even though the source is passed as an absolute path.
const WARN_DIAGNOSTIC: &str = "warn.erl:3:8: Warning: variable 'X' is unused\n%    3| f() -> X = 1, ok.\n%     |        ^\n\n";

/// The compiler's own diagnostic for [`BERR_ERL`], byte for byte, cross-checked against the
/// system `erlc`'s: the undefined function first, then the unbound variable, each with its
/// excerpt and a blank line after it.
const BERR_DIAGNOSTIC: &str = "berr.erl:3:8: function unused/1 undefined\n%    3| f() -> unused(X), ok.\n%     |        ^\n\nberr.erl:3:15: variable 'X' is unbound\n%    3| f() -> unused(X), ok.\n%     |               ^\n\n";

// ---------------------------------------------------------------------------------------
// Bounded child execution (the house pattern, with a longer bound for a booting `erlc`)
// ---------------------------------------------------------------------------------------

/// How long one `erlc` child may run. The route boots this runtime, loads the preloaded set
/// and then runs the real compiler, so ≈1.5 s on an idle host and several times that on a
/// loaded one — the shared 30 s probe bound is deliberately too tight for it.
const ERLC_TIMEOUT: Duration = Duration::from_secs(180);

/// How often a running child's exit is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One finished child, its streams kept **raw** (a diagnostic's exact bytes are asserted).
struct Run {
    code: i32,
    out: Vec<u8>,
    err: Vec<u8>,
}

impl Run {
    /// `stdout` as text, for the assertion messages.
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    /// `stderr` as text, for the assertion messages.
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.err).into_owned()
    }
}

/// The environment names every child removes before its own variables are applied: the whole
/// `ERL_*FLAGS` family (an inherited flag would otherwise land on both sides of a comparison
/// with different meanings) plus `ERLC_EMULATOR` (so the reference row can never be
/// redirected to this runtime by the host).
fn stripped_names() -> Vec<String> {
    let mut names = vec![
        "ERL_AFLAGS".to_string(),
        "ERL_FLAGS".to_string(),
        "ERL_ZFLAGS".to_string(),
        "ERLC_EMULATOR".to_string(),
        beam_rt::cli::otp_flags_var(),
    ];
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str() {
            if name.starts_with("ERL_OTP") && name.ends_with("_FLAGS") {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Runs `program args…` with `cwd`, the stripped environment plus `env`, bounded by
/// [`ERLC_TIMEOUT`].
///
/// A child that cannot be spawned, polled or collected — or that outlives the bound — fails
/// the test: the toolchain's *presence* is a skip, but a toolchain that hangs is a defect
/// this suite must report rather than swallow.
fn run_for(program: &Path, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Run {
    let stripped = stripped_names();
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &stripped {
        command.env_remove(name);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap_or_else(|error| {
        panic!(
            "cannot spawn `{} {}`: {error}",
            program.display(),
            args.join(" ")
        )
    });
    let deadline = Instant::now() + ERLC_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().unwrap_or_else(|error| {
                panic!("cannot collect `{}`'s output: {error}", program.display())
            }),
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{} {}` outlived the {}s bound and was killed",
                    program.display(),
                    args.join(" "),
                    ERLC_TIMEOUT.as_secs()
                );
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: output.stdout,
        err: output.stderr,
    }
}

// ---------------------------------------------------------------------------------------
// The scratch directory and the toolchain
// ---------------------------------------------------------------------------------------

/// One test's scratch directory, removed on drop.
///
/// Unique per call (label + pid + a per-process sequence) because concurrent test *processes*
/// share `$TEMP`; `src/context/test_support.rs`'s `scratch_dir` is `pub(crate)` and is not
/// reachable from an integration test, so the helper lives here (the
/// `tests/code_delete_live.rs` pattern).
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// A fresh, empty scratch directory named after `label`.
    fn new(label: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_erlc_{label}_{}_{sequence}",
            std::process::id()
        ));
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot clear {}: {error}", dir.display()),
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            panic!("cannot prepare {}: {error}", dir.display());
        }
        Self { dir }
    }

    /// The directory itself.
    fn path(&self) -> &Path {
        &self.dir
    }

    /// A subdirectory (created).
    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        if let Err(error) = std::fs::create_dir_all(&path) {
            panic!("cannot prepare {}: {error}", path.display());
        }
        path
    }

    /// Writes `contents` to `name` and hands back its path.
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.join(name);
        if let Err(error) = std::fs::write(&path, contents) {
            panic!("cannot write {}: {error}", path.display());
        }
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the scratch dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// The built runtime binary under test.
fn beam_rt() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_beam_rt"))
}

/// The compiler under test, or `None` after a loud skip.
macro_rules! require_erlc {
    () => {
        match resolve_erlc() {
            Some(erlc) => erlc,
            None => {
                support::skip_loudly("erlc_emulator", "no `erlc` on $PATH");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------------------
// The shared rows
// ---------------------------------------------------------------------------------------

/// Compiles `source` **through this runtime** and through the **system** `erlc`, into two
/// separate output directories, and returns the two finished children plus the two artifact
/// paths.
///
/// The source path is the same for both runs, so the artifacts are comparable byte for byte;
/// only the output directory and the `ERLC_EMULATOR` differ.
fn compile_both(
    erlc: &Path,
    scratch: &Scratch,
    source: &Path,
    extra: &[&str],
) -> (Run, Run, PathBuf, PathBuf) {
    let ours_dir = scratch.subdir("ours");
    let theirs_dir = scratch.subdir("theirs");
    let emulator = beam_rt().to_str().expect("the binary path is UTF-8");
    let source_text = source.to_str().expect("the scratch path is UTF-8");

    // Both runs take the same options and the same source path; only `-o` and the emulator
    // differ, so the two artifacts are comparable byte for byte.
    let args = |out: &Path| -> Vec<String> {
        let mut args: Vec<String> = extra.iter().map(|arg| arg.to_string()).collect();
        args.push("-o".to_string());
        args.push(out.to_str().expect("the scratch path is UTF-8").to_string());
        args.push(source_text.to_string());
        args
    };
    let call = |out: &Path, env: &[(&str, &str)]| {
        let args = args(out);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        run_for(erlc, &borrowed, scratch.path(), env)
    };
    let ours = call(&ours_dir, &[("ERLC_EMULATOR", emulator)]);
    let theirs = call(&theirs_dir, &[]);

    let module = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("the fixture is a named file");
    (
        ours,
        theirs,
        ours_dir.join(format!("{module}.beam")),
        theirs_dir.join(format!("{module}.beam")),
    )
}

/// Asserts the three stream claims of a successful compile: exit 0, no stdout, no stderr.
fn assert_silent_success(run: &Run, what: &str) {
    assert_eq!(
        run.code,
        0,
        "{what} must exit 0 — stderr:\n{}",
        run.stderr()
    );
    assert_eq!(
        run.out,
        Vec::<u8>::new(),
        "{what} must write nothing to stdout, got:\n{}",
        run.stdout()
    );
    assert_eq!(
        run.err,
        Vec::<u8>::new(),
        "{what} must write nothing to stderr, got:\n{}",
        run.stderr()
    );
}

/// Asserts the reference artifact exists, and that the artifact this runtime produced is
/// **byte-identical** to it.
///
/// The comparison is `std::fs::read` + `assert_eq!` (never a `cmp` child, which is not
/// guaranteed to be installed and would only report "differ" instead of the byte count).
fn assert_identical_artifacts(ours: &Path, theirs: &Path) {
    let ours_bytes = std::fs::read(ours)
        .unwrap_or_else(|error| panic!("no artifact at {}: {error}", ours.display()));
    let theirs_bytes = std::fs::read(theirs)
        .unwrap_or_else(|error| panic!("no reference artifact at {}: {error}", theirs.display()));
    assert!(
        !theirs_bytes.is_empty(),
        "the system `erlc` produced an empty {}",
        theirs.display()
    );
    assert_eq!(
        ours_bytes.len(),
        theirs_bytes.len(),
        "`{}` is {} bytes where the system compiler's is {}",
        ours.display(),
        ours_bytes.len(),
        theirs_bytes.len()
    );
    assert_eq!(
        ours_bytes,
        theirs_bytes,
        "`{}` differs from the system compiler's `{}`",
        ours.display(),
        theirs.display()
    );
}

/// Loads `export` of the module compiled into `dir` under the **oracle** and returns the
/// line it printed.
///
/// `oracle_eval` appends `-s init stop` (its documented backstop) and the expression also
/// ends in its own `halt(0).`, which is the shape every probe in these suites uses.
fn oracle_calls(oracle: &Path, dir: &Path, module: &str, export: &str) -> String {
    let expr = format!("io:format(\"~p~n\",[{module}:{export}]), halt(0).");
    support::oracle_eval(oracle, Some(dir), &expr).unwrap_or_else(|| {
        panic!("the oracle could not load {module} from {} to answer {export}", dir.display())
    })
}

// ---------------------------------------------------------------------------------------
// (a) the plain `erlc` route
// ---------------------------------------------------------------------------------------

/// `ERLC_EMULATOR=<beam_rt> erlc -o A ecap.erl` really compiles: exit 0, **empty stdout and
/// stderr**, an artifact in `A`, and those bytes identical to the system `erlc`'s artifact
/// for the same source.
///
/// This is the whole front end — `erl_compile`'s command-line driver, the compiler itself,
/// `epp`, and the `file:`/`code:lib_dir/2` round trips in between — running on the Rust
/// runtime, and it is the row that would fail first if the compact `+`-flag group or the
/// `-extra` plain-argument split regressed.
#[test]
fn the_erlc_front_end_compiles_through_this_runtime_byte_identically() {
    let erlc = require_erlc!();
    let scratch = Scratch::new("plain");
    let source = scratch.write("ecap.erl", ECAP_ERL);

    let (ours, theirs, ours_beam, theirs_beam) = compile_both(&erlc, &scratch, &source, &[]);
    assert_silent_success(&ours, "`erlc -o A ecap.erl` through this runtime");
    assert_silent_success(&theirs, "`erlc -o B ecap.erl` through the system compiler");
    assert!(
        ours_beam.is_file(),
        "no {} was produced — the compile reported success",
        ours_beam.display()
    );
    assert_identical_artifacts(&ours_beam, &theirs_beam);
}

/// The same three claims for a **option-carrying** invocation: `+deterministic` (an `erlc`
/// `+` option passed straight to the compiler) and `-I INC`, with a source that pulls in
/// `kernel/include/file.hrl` through `-include_lib/1` and `hdrs.hrl` through `-include/1`.
///
/// The two include forms take two different routes inside the compiler — `code:lib_dir/2`
/// for the first, the option path for the second — so this row is what pins that the route
/// carries the front end's options faithfully, not just the source file.
#[test]
fn the_option_carrying_erlc_route_compiles_byte_identically() {
    let erlc = require_erlc!();
    let scratch = Scratch::new("deterministic");
    let include = scratch.subdir("inc");
    let header = include.join("hdrs.hrl");
    if let Err(error) = std::fs::write(&header, HDRS_HRL) {
        panic!("cannot write {}: {error}", header.display());
    }
    let source = scratch.write("finfo.erl", FINFO_ERL);
    let include_text = include.to_str().expect("the scratch path is UTF-8");

    let (ours, theirs, ours_beam, theirs_beam) = compile_both(
        &erlc,
        &scratch,
        &source,
        &["+deterministic", "-I", include_text],
    );
    assert_silent_success(
        &ours,
        "`erlc +deterministic -I INC -o A finfo.erl` through this runtime",
    );
    assert_silent_success(
        &theirs,
        "`erlc +deterministic -I INC -o B finfo.erl` through the system compiler",
    );
    assert!(
        ours_beam.is_file(),
        "no {} was produced — the compile reported success",
        ours_beam.display()
    );
    assert_identical_artifacts(&ours_beam, &theirs_beam);
}

// ---------------------------------------------------------------------------------------
// (c) the artifacts are real `.beam`s
// ---------------------------------------------------------------------------------------

/// Both artifacts this runtime produced are loaded by the **oracle** and answer their
/// exports: `ecap:double(21)` ⇒ `42` and `finfo:greet()` ⇒ `hello`.
///
/// Byte equality with the system compiler's output already implies this, but only if the
/// system compiler's output is loadable — asserting it directly is what makes the claim
/// "this is a real `.beam`, not merely matching bytes" instead of an inference.
#[test]
fn the_compiled_artifacts_load_and_run_under_the_oracle() {
    let Some(oracle) = support::require_oracle("erlc_emulator") else {
        return;
    };
    let erlc = require_erlc!();
    let scratch = Scratch::new("loadable");

    let ecap = scratch.write("ecap.erl", ECAP_ERL);
    let ecap_ours = scratch.subdir("ecap_ours");
    let ecap_dir = ecap_ours.to_str().expect("the scratch path is UTF-8");
    let ecap_source = ecap.to_str().expect("the scratch path is UTF-8");
    let emulator = beam_rt().to_str().expect("the binary path is UTF-8");
    let compiled = run_for(
        &erlc,
        &["-o", ecap_dir, ecap_source],
        scratch.path(),
        &[("ERLC_EMULATOR", emulator)],
    );
    assert_silent_success(&compiled, "`erlc -o A ecap.erl` through this runtime");
    let answer = oracle_calls(&oracle, &ecap_ours, "ecap", "double(21)");
    assert_eq!(answer, "42", "the oracle loading the artifact this runtime emitted");

    let include = scratch.subdir("inc");
    if let Err(error) = std::fs::write(include.join("hdrs.hrl"), HDRS_HRL) {
        panic!("cannot write the include file: {error}");
    }
    let include_text = include.to_str().expect("the scratch path is UTF-8");
    let finfo = scratch.write("finfo.erl", FINFO_ERL);
    let finfo_ours = scratch.subdir("finfo_ours");
    let finfo_dir = finfo_ours.to_str().expect("the scratch path is UTF-8");
    let finfo_source = finfo.to_str().expect("the scratch path is UTF-8");
    let compiled = run_for(
        &erlc,
        &["+deterministic", "-I", include_text, "-o", finfo_dir, finfo_source],
        scratch.path(),
        &[("ERLC_EMULATOR", emulator)],
    );
    assert_silent_success(
        &compiled,
        "`erlc +deterministic -I INC -o A finfo.erl` through this runtime",
    );
    let answer = oracle_calls(&oracle, &finfo_ours, "finfo", "greet()");
    assert_eq!(answer, "hello", "the oracle loading the artifact this runtime emitted");
}

// ---------------------------------------------------------------------------------------
// (d) a diagnostic-producing source: byte-identical to the system `erlc`
// ---------------------------------------------------------------------------------------

/// A source that makes the compiler emit a **warning** compiles through this runtime exactly
/// as it does under the system `erlc`: exit **0**, empty stdout, the same **90-byte** stderr
/// (the compiler's own diagnostic, [`WARN_DIAGNOSTIC`]) and the same `.beam` artifact.
///
/// This is the diagnostic *printer* running on the Rust runtime: `compile:report_warnings/1`
/// reaches the compiler's own `lists:flatmap/2` closure over the running slice, so a warning
/// lands on stderr character for character. Both halves are pinned twice — against the
/// golden const *and* against the live system `erlc`'s own stderr, byte for byte — so a
/// wording drift on either side fails here; a fragment assertion would let it through.
///
/// The artifact is pinned too, because a warning is not an error: `warn.beam` is emitted and
/// is byte-identical to the system compiler's. (A source with real compile *errors* leaves
/// nothing behind — see [`a_compile_error_is_reported_byte_identically_and_leaves_no_artifact`].)
#[test]
fn a_compiler_warning_is_printed_byte_identically_and_the_artifact_is_emitted() {
    let erlc = require_erlc!();
    let scratch = Scratch::new("warning");
    let source = scratch.write("warn.erl", WARN_ERL);

    let (ours, theirs, ours_beam, theirs_beam) = compile_both(&erlc, &scratch, &source, &[]);

    assert_eq!(
        ours.code, 0,
        "a warning-only source must compile — stderr:\n{}",
        ours.stderr()
    );
    assert_eq!(
        ours.out,
        Vec::<u8>::new(),
        "the warning route must write nothing to stdout, got:\n{}",
        ours.stdout()
    );
    assert_eq!(
        ours.stderr(),
        WARN_DIAGNOSTIC,
        "the warning route's stderr, verbatim ({} bytes)",
        ours.err.len()
    );
    assert_eq!(
        ours.err.len(),
        90,
        "the warning diagnostic is 90 bytes ({} here):\n{}",
        ours.err.len(),
        ours.stderr()
    );

    // A warning is not an error: the artifact is emitted, and this runtime's compiler emitted
    // the same bytes as the system one.
    assert!(
        ours_beam.is_file(),
        "a warning-only source emits {}",
        ours_beam.display()
    );
    assert_identical_artifacts(&ours_beam, &theirs_beam);

    // The reference half: the system `erlc` compiles the same source and prints the *same*
    // diagnostic, byte for byte.
    assert_eq!(
        theirs.code,
        0,
        "the system `erlc` must compile a warning-only source — stderr:\n{}",
        theirs.stderr()
    );
    assert_eq!(
        theirs.out,
        Vec::<u8>::new(),
        "the system `erlc` writes nothing to stdout, it printed:\n{}",
        theirs.stdout()
    );
    assert!(
        theirs.stderr().contains("Warning: variable 'X' is unused"),
        "the diagnostic names the warning, the system `erlc` printed:\n{}",
        theirs.stderr()
    );
    assert_eq!(
        theirs.err.len(),
        WARN_DIAGNOSTIC.len(),
        "the system `erlc`'s diagnostic is {} bytes, the golden is {}:\n{}",
        theirs.err.len(),
        WARN_DIAGNOSTIC.len(),
        theirs.stderr()
    );
    assert_eq!(
        ours.err,
        theirs.err,
        "this runtime's diagnostic must be the system `erlc`'s, byte for byte\n\
         ours:\n{}\ntheirs:\n{}",
        ours.stderr(),
        theirs.stderr()
    );
}

/// A source with real **compile errors** is reported exactly as under the system `erlc`:
/// exit **1**, empty stdout, the same **184-byte** stderr (both errors, [`BERR_DIAGNOSTIC`])
/// and **no** `.beam` on either side.
///
/// The compiler never reaches emission here, so the artifact is absent on both sides — that
/// is what the pair with
/// [`a_compiler_warning_is_printed_byte_identically_and_the_artifact_is_emitted`] pins: a
/// warning still emits, an error emits nothing, and both diagnostics are byte-identical to
/// the system compiler's.
#[test]
fn a_compile_error_is_reported_byte_identically_and_leaves_no_artifact() {
    let erlc = require_erlc!();
    let scratch = Scratch::new("error");
    let source = scratch.write("berr.erl", BERR_ERL);

    let (ours, theirs, ours_beam, theirs_beam) = compile_both(&erlc, &scratch, &source, &[]);

    assert_eq!(
        ours.code, 1,
        "a source with compile errors must fail — stderr:\n{}",
        ours.stderr()
    );
    assert_eq!(
        ours.out,
        Vec::<u8>::new(),
        "the error route must write nothing to stdout, got:\n{}",
        ours.stdout()
    );
    assert_eq!(
        ours.stderr(),
        BERR_DIAGNOSTIC,
        "the error route's stderr, verbatim ({} bytes)",
        ours.err.len()
    );
    assert_eq!(
        ours.err.len(),
        184,
        "the error diagnostic is 184 bytes ({} here):\n{}",
        ours.err.len(),
        ours.stderr()
    );
    assert!(
        !ours_beam.exists(),
        "a compile error leaves no artifact, but {} exists",
        ours_beam.display()
    );

    // The reference half: the system `erlc` reports the same two errors, writes nothing to
    // stdout and writes no artifact either.
    assert_eq!(
        theirs.code,
        1,
        "the system `erlc` must fail on a compile error — stderr:\n{}",
        theirs.stderr()
    );
    assert_eq!(
        theirs.out,
        Vec::<u8>::new(),
        "the system `erlc` writes nothing to stdout, it printed:\n{}",
        theirs.stdout()
    );
    assert!(
        theirs.stderr().contains("function unused/1 undefined"),
        "the diagnostic names the undefined function, the system `erlc` printed:\n{}",
        theirs.stderr()
    );
    assert_eq!(
        theirs.err.len(),
        BERR_DIAGNOSTIC.len(),
        "the system `erlc`'s diagnostic is {} bytes, the golden is {}:\n{}",
        theirs.err.len(),
        BERR_DIAGNOSTIC.len(),
        theirs.stderr()
    );
    assert_eq!(
        ours.err,
        theirs.err,
        "this runtime's diagnostic must be the system `erlc`'s, byte for byte\n\
         ours:\n{}\ntheirs:\n{}",
        ours.stderr(),
        theirs.stderr()
    );
    assert!(
        !theirs_beam.exists(),
        "the system `erlc` leaves no artifact on a compile error, but {} exists",
        theirs_beam.display()
    );
}
