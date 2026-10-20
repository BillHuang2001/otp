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
//! The shared harness for the three **A.3 end-to-end** suites
//! (`tests/nif_a3_{thread_env,schedule,resource_stop}.rs`).
//!
//! Each of them does the same four things, and those four things are here so the suites
//! cannot drift apart:
//!
//! 1. **Find the host toolchain** — the oracle `erl`, `erlc`, a C compiler and a real
//!    `erl_nif.h`. A missing tool is a **loud skip** ([`support::skip_loudly`]), never a
//!    silent pass; a tool that is present but *fails* (a `cc` error, an `erlc` error) is a
//!    **panic**, because that is not "the facility is absent", it is a broken fixture.
//! 2. **Compile a real C NIF fixture in place** — the `.c` sources belong to the sibling
//!    crate (`../beam_bifs/tests/fixtures/a3_*.c`) and are compiled **from their own path**
//!    into a private scratch directory. They are never copied and never modified.
//! 3. **Drive it from interpreted Erlang on both runtimes** — the same `.beam` files, the
//!    same `go/1` driver, the same scratch directory: once through
//!    `beam_rt run <driver.beam> <driver>:go/1 "<load-path>"` and once through
//!    `erl -noshell -pa <dir> -eval '<driver>:go("<load-path>"), halt(0).' -s init stop`.
//! 4. **Compare the two transcripts line by line**, pinning *both* sides to measured
//!    literals and naming every divergence.
//!
//! # Why the driver is a separate module from the NIF module
//!
//! `erlang:load_nif/2` requires the calling module to be the name in the C's
//! `ERL_NIF_INIT`, so the module that loads the library *is* `a3_thread_env` (etc.) and has
//! to declare a stub for **every** row of the C's `ErlNifFunc` table (`erl_nif.c:4874-4878`
//! refuses the whole load with `{error,{bad_lib,"Function not found M:F/A"}}` otherwise).
//! A call to one of those stubs from *inside that same module* is compiled to the `call`
//! opcode, which the engine resolves in-machine and never routes through
//! [`RuntimeContext::call_bif`](beam_rt::RuntimeContext) — so it would keep running the
//! stub instead of the NIF. The driver is therefore a **second** module whose calls are
//! `call_ext`, exactly as `tests/nif.rs` does it, and it is the same shape the oracle sees.
//!
//! # Why `-on_load` is not used
//!
//! The fixtures' own `.erl` drivers load the library from `-on_load`, which this runtime
//! does not run (`beam_instr::OnLoad` is a no-op here). Rather than skip the fixtures, the
//! suites compile their own stub module with an explicit `init/1` and call it from the
//! driver: the *same* C library, the same load path, the same `erlang:load_nif/2` call —
//! only the trigger differs. The fixture `.erl` files remain the property of `beam_bifs`
//! and are not used, copied or changed here.
//!
//! # Why the transcripts are text
//!
//! Both sides print Erlang terms with `io:format("...~w~n")`, and this runtime's `~w`
//! agrees with the oracle's on every shape these fixtures produce (integers, atoms,
//! tuples, lists and binaries — `~w` renders a binary as `<<97,98,99>>`, which both do
//! identically), so a term-level comparison needs no parser. Values that are genuinely
//! host- or address-dependent (a raw NIF term word, a file descriptor, a stack-word count)
//! are printed on lines named `note_…` and are **never** compared — but they are named
//! [`NOTES`]-style so a reader of the test output can see them.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::support;

/// A host toolchain able to build and drive a real C NIF.
pub struct Toolchain {
    /// The oracle VM (`$ERTS_ORACLE`, else `erl` on `$PATH`).
    pub erl: PathBuf,
    /// The host `erlc`.
    pub erlc: PathBuf,
    /// The C compiler (`$CC`, else `cc`).
    pub cc: String,
    /// The directory holding the real `erl_nif.h`.
    pub include: PathBuf,
}

/// Resolve the whole toolchain, or `None` after a **loud** skip naming the missing tool.
///
/// Only an *absent* tool skips. A tool that is present and then fails to do its job is a
/// panic in [`build`] — the fixtures are then broken, not unavailable.
pub fn toolchain(banner: &str) -> Option<Toolchain> {
    let Some(erl) = support::require_oracle(banner) else {
        return None;
    };
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly(banner, "no `erlc` on $PATH");
        return None;
    };
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let compiles = Command::new(&cc)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !compiles {
        support::skip_loudly(banner, &format!("no C compiler (`{cc}`, or $CC)"));
        return None;
    }
    let Some(include) = erl_include_dir(&erl) else {
        support::skip_loudly(
            banner,
            "no `erl_nif.h` under the oracle's `code:root_dir()` (or its `erts-*` tree)",
        );
        return None;
    };
    Some(Toolchain {
        erl,
        erlc,
        cc,
        include,
    })
}

/// The directory holding the host `erl_nif.h`, derived from the **oracle**.
///
/// `code:lib_dir(erts, include)` is *not* a reliable answer: on this host it returns a path
/// that does not exist, which is why the root is resolved first and only real directories
/// are accepted. Candidates, in order: `<root>/usr/include` (the installed-headers
/// location), every `<root>/erts-*/include`, and finally the (usually dead)
/// `code:lib_dir(erts, include)` — each one is only taken when it really holds the header.
fn erl_include_dir(erl: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(root) = support::oracle_eval(erl, None, "io:format(\"~s\",[code:root_dir()])") {
        let root = PathBuf::from(root);
        candidates.push(root.join("usr").join("include"));
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("erts-") {
                    candidates.push(entry.path().join("include"));
                }
            }
        }
    }
    if let Some(lib_dir) = support::oracle_eval(
        erl,
        None,
        "io:format(\"~s\",[code:lib_dir(erts,include)])",
    ) {
        candidates.push(PathBuf::from(lib_dir));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("erl_nif.h").is_file())
}

/// A compiled fixture: one `.so`, its stub module and the driver, all in one scratch
/// directory (the `.so` must sit next to the `.beam` that loads it — `load_nif/2` resolves
/// a bare name relative to the module's own directory).
pub struct Fixture {
    /// The private scratch directory (`<tmp>/beam_rt_nif_a3_<label>_<pid>_<n>`).
    pub dir: PathBuf,
    /// The compiled driver `.beam` — the `run` action's file argument.
    pub driver: PathBuf,
    /// The driver's module name (`<driver>:go/1`).
    pub driver_module: String,
    /// The NIF module name (the `.so` is `<dir>/<module>.so`).
    pub module: String,
}

impl Fixture {
    /// The path handed to `erlang:load_nif/2` — the `.so` **stem**, since the loader appends
    /// the shared-object suffix itself.
    pub fn load_path(&self) -> PathBuf {
        self.dir.join(&self.module)
    }

    /// Removes the scratch directory. The compiled artifacts are not source; a failure to
    /// remove them is reported rather than hidden.
    pub fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// Runs the driver through the **built binary** — the same invocation a user would make:
    ///
    /// ```text
    /// beam_rt -noshell -pa <dir> run <driver.beam> <driver>:go/1 "<load-path>"
    /// ```
    ///
    /// The load path is a **double-quoted** `run` argument because the `run` grammar parses a
    /// bare token as an atom and only a quoted one as an Erlang char list, and
    /// `erlang:load_nif/2` needs a list or a binary (`badarg` for an atom). The scratch path
    /// carries no `"`, so no escaping is needed. The child is bounded by
    /// [`support::PROBE_TIMEOUT`], so a driver that blocks fails the suite instead of hanging
    /// it.
    fn run_rust(&self) -> Output {
        let dir = self.dir.display().to_string();
        let driver = self.driver.display().to_string();
        let mfa = format!("{}:go/1", self.driver_module);
        let load = format!("\"{}\"", self.load_path().display());
        let args = ["-noshell", "-pa", &dir, "run", &driver, &mfa, &load];
        match support::run_bounded(Path::new(env!("CARGO_BIN_EXE_beam_rt")), &args) {
            Some(output) => output,
            None => panic!(
                "`beam_rt run {}` did not answer within the {}s probe bound",
                self.driver.display(),
                support::PROBE_TIMEOUT.as_secs()
            ),
        }
    }

    /// The `beam_rt` transcript: the driver's `key=value` lines, asserting a clean exit.
    pub fn rust_transcript(&self) -> Observed {
        let output = self.run_rust();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "the rust runtime exited with {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code()
        );
        assert!(
            stderr.is_empty(),
            "the `run` action must be silent on stderr\n--- stderr ---\n{stderr}"
        );
        Observed::parse(&stdout)
    }

    /// The oracle transcript, from the **same** `.beam` files and the same scratch directory:
    ///
    /// ```text
    /// erl -noshell -pa <dir> -eval '<driver>:go("<load-path>"), halt(0).' -s init stop
    /// ```
    ///
    /// (`-s init stop` is [`support::oracle_eval`]'s terminator, so the node always exits.)
    /// A probe that cannot answer is a panic: the oracle was found, so a probe that fails is
    /// a broken fixture, never a reason to skip.
    pub fn oracle_transcript(&self, tc: &Toolchain) -> Observed {
        let expr = format!(
            "{}:go(\"{}\"), halt(0).",
            self.driver_module,
            self.load_path().display()
        );
        match support::oracle_eval(&tc.erl, Some(&self.dir), &expr) {
            Some(stdout) => Observed::parse(&stdout),
            None => panic!(
                "the oracle could not answer `{}` — see the warning above.\n\
                 The fixture's C source, its stub module and its driver are all in {}.",
                expr,
                self.dir.display()
            ),
        }
    }
}

/// Compiles a fixture and its Erlang modules into a fresh scratch directory.
///
/// `c_source` is the **sibling crate's** fixture, compiled where it lives (never copied);
/// `sources` are the `(module name, source text)` pairs to write and compile — the stub
/// module whose name matches the C's `ERL_NIF_INIT`, and the driver.
///
/// Returns `None` only when the fixture's own `.c` source is missing.
pub fn build(
    tc: &Toolchain,
    label: &str,
    c_source: &Path,
    module: &str,
    sources: &[(&str, &str)],
    driver_module: &str,
) -> Option<Fixture> {
    if !c_source.is_file() {
        support::skip_loudly(
            &format!("nif_a3/{label}"),
            &format!("the fixture source {} is missing", c_source.display()),
        );
        return None;
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_nif_a3_{label}_{}_{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", dir.display()));

    // The shared object, built from the fixture's own path:
    //   cc -shared -fPIC -I<include> -o <dir>/<module>.so <fixture>/<module>.c
    let output = Command::new(&tc.cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", tc.include.display()))
        .arg("-o")
        .arg(dir.join(format!("{module}.so")))
        .arg(c_source)
        .output()
        .unwrap_or_else(|error| panic!("cannot run the C compiler `{}`: {error}", tc.cc));
    assert!(
        output.status.success(),
        "`{} -shared -fPIC -I{} -o {}.so {}` failed ({:?}):\n{}",
        tc.cc,
        tc.include.display(),
        module,
        c_source.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    // The Erlang modules: `erlc -o <dir> <dir>/<stem>.erl …`.
    let mut paths = Vec::with_capacity(sources.len());
    for (stem, source) in sources {
        let path = dir.join(format!("{stem}.erl"));
        std::fs::write(&path, source)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
        paths.push(path);
    }
    let output = Command::new(&tc.erlc)
        .arg("-o")
        .arg(&dir)
        .args(&paths)
        .output()
        .unwrap_or_else(|error| panic!("cannot run `erlc`: {error}"));
    assert!(
        output.status.success(),
        "`erlc -o {} {:?}` failed ({:?}):\n{}",
        dir.display(),
        paths,
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let driver = dir.join(format!("{driver_module}.beam"));
    assert!(
        driver.is_file(),
        "`erlc` produced no {} (a module name that does not match its file?)",
        driver.display()
    );
    Some(Fixture {
        dir,
        driver,
        driver_module: driver_module.to_string(),
        module: module.to_string(),
    })
}

/// One side's `key=value` transcript.
pub struct Observed {
    /// The pairs, in the order the driver printed them.
    pairs: Vec<(String, String)>,
}

impl Observed {
    /// Parses the driver's `key=value` lines, in order. Any other line (the `run` action's
    /// own printed result, a banner) is ignored: the transcript is exactly the `k=v` lines.
    pub fn parse(stdout: &str) -> Self {
        let pairs = stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect();
        Self { pairs }
    }

    /// The value printed under `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Every compared key, sorted — the `note_…` lines are excluded by construction: they
    /// carry host- or address-dependent figures (a raw term word, a stack-word count, a
    /// file descriptor) that neither side can agree on and that no test may pretend to.
    pub fn compared_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .pairs
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !name.starts_with("note_"))
            .collect();
        keys.sort();
        keys
    }

    /// Prints the `note_…` lines — recorded, never asserted.
    pub fn print_notes(&self, label: &str) {
        for (name, value) in &self.pairs {
            if name.starts_with("note_") {
                println!("{label}: {name} = {value} (recorded, not compared)");
            }
        }
    }
}

/// One row of a suite's expectation table.
pub struct Expect {
    /// The driver's line name.
    pub key: &'static str,
    /// What the **oracle** prints for it — the measured ground truth, pinned literally so a
    /// changed fixture or a different host cannot pass unnoticed.
    pub oracle: &'static str,
    /// `None` when the rust runtime must agree with the oracle; `Some((value, why))` when it
    /// answers a **documented** different value, with the reason it does.
    pub diverges: Option<(&'static str, &'static str)>,
}

/// Compares the two transcripts against `expected`, printing the per-key outcome.
///
/// Fails (panics) when
/// * the two sides do not print the **same set of keys** (a dropped or added line),
/// * a key is missing from `expected` (an unasserted observation), or
/// * any value is not the pinned one — including a diverging key that no longer diverges,
///   which would mean a documented gap silently changed shape.
pub fn check(label: &str, expected: &[Expect], rust: &Observed, oracle: &Observed) {
    let mut want: Vec<String> = expected
        .iter()
        .map(|row| row.key.to_string())
        .collect::<Vec<_>>();
    want.sort();
    assert_eq!(
        oracle.compared_keys(),
        want,
        "{label}: the oracle transcript's keys do not match the suite's expectation table — \
         the driver printed a different set of observations"
    );
    assert_eq!(
        rust.compared_keys(),
        want,
        "{label}: the rust transcript's keys do not match the suite's expectation table"
    );

    let mut agreeing = 0usize;
    let mut diverging: Vec<&'static str> = Vec::new();
    for row in expected {
        let oracle_value = oracle
            .get(row.key)
            .unwrap_or_else(|| panic!("{label}: the oracle printed no `{}`", row.key));
        assert_eq!(
            oracle_value, row.oracle,
            "{label}: the ORACLE's `{}` is not the measured ground truth — the fixture, the \
             host, or the driver changed",
            row.key
        );
        let rust_value = rust
            .get(row.key)
            .unwrap_or_else(|| panic!("{label}: rust printed no `{}`", row.key));
        match row.diverges {
            None => {
                assert_eq!(
                    rust_value, row.oracle,
                    "{label}: `{}` disagrees with the oracle ({oracle_value:?} vs \
                     {rust_value:?}); this row is not a documented divergence",
                    row.key
                );
                println!("{label}: {} = {rust_value}  (agrees with the oracle)", row.key);
                agreeing += 1;
            }
            Some((documented, why)) => {
                assert_eq!(
                    rust_value, documented,
                    "{label}: `{}` is a documented divergence and must keep its documented \
                     value; the mechanism is: {why}",
                    row.key
                );
                println!(
                    "{label}: {} = {rust_value}  (DIVERGES from the oracle's {oracle_value}: {why})",
                    row.key
                );
                diverging.push(row.key);
            }
        }
    }
    println!(
        "{label}: {} observations, {} agree exactly, {} documented divergence(s) {diverging:?}",
        expected.len(),
        agreeing,
        diverging.len()
    );
    assert!(
        !diverging.is_empty() || agreeing == expected.len(),
        "{label}: bookkeeping error"
    );
}
