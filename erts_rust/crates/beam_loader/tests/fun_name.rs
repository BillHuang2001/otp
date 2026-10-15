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

//! Oracle cross-check that [`FunDescriptor::function`](beam_loader::FunDescriptor::function)
//! is the `FunT` chunk's **function atom** — the name `erlang:fun_info(F, name)`
//! reports.
//!
//! # What this proves
//!
//! The `FunT` entry's first field is the lambda's function atom, which
//! `beam_dict:lambda_table/1` (`../../lib/compiler/src/beam_dict.erl`) resolves
//! through the module's local/export label tables: the atom of the function whose
//! **body label** the lambda's `label` names. It is therefore not "the name of an
//! anonymous function" in general — a *named* local fun (`fun mk/1`, `mk/1` being a
//! local, non-exported function) carries the plain atom `mk`. The runtime reports
//! that very atom: `emu_load.c` registers the entry with the lambda's function atom
//! (`erts_fun_entry_put` + the wrapping function's `func_info`), and `fun_info_2`'s
//! `name` item answers the code's MFA for it (`erl_bif_info.c:4007`).
//!
//! The test therefore compiles a scratch `fiprobe` module with the **system** `erlc`,
//! loads the resulting `.beam` through this crate's loader, asks the **system** `erl`
//! for `element(2, erlang:fun_info(F, name))` of each fixture fun — one
//! machine-parseable line per fun — and asserts that the descriptor's `function` atom
//! **names exactly the oracle's string**. The descriptors are located by `index` (the
//! module-unique `FunT` index `erl_bif_info.c`'s `index` item reports) and confirmed
//! by `uniq`, so the name never takes part in the matching (no circularity).
//!
//! **Failure contract:** the test skips **loudly** (`eprintln!("SKIP: …")` + return)
//! when the system `erl`/`erlc` toolchain is unavailable or rejects the fixture —
//! never a silent, vacuous pass. Every other outcome asserts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use beam_loader::{load_file, FunDescriptor};

/// The scratch fixture, inlined (compiled with the system `erlc` at test time).
///
/// * `named/0` returns a **named** local fun — `mk/1` is a local, non-exported
///   function, so the lambda's body label is `mk`'s own and the `FunT` function
///   atom is the plain atom `mk`.
/// * `anon/1` returns a genuine **anonymous** lambda, whose compiler-generated
///   wrapper function is `'-anon/1-fun-0-'`.
const FIXTURE_SOURCE: &str = "\
-module(fiprobe).
-export([named/0, anon/1]).

mk(X) -> X + 1.

named() -> fun mk/1.

anon(X) -> fun(Y) -> X + Y end.
";

/// The two fixture funs, as `(old index, name)` — the pinned facts the live oracle
/// re-derives below. `index` is the module-unique `FunT` index (`fun_info(F, index)`);
/// the anonymous lambda's generated name is verified byte-for-byte against the
/// oracle, and pinned here so a change in what `erlc` emits cannot slip through.
const PINNED: &[(u32, &str)] = &[(0, "mk"), (1, "-anon/1-fun-0-")];

/// The wall-clock bound for one oracle `erl`/`erlc` invocation.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Oracle resolution (mirrors `beam_bifs/tests/fun_info_whole_list.rs`)
// ---------------------------------------------------------------------------

/// Whether `path` is an executable file (`$ERTS_ORACLE` may point at one).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The system `erl`: `$ERTS_ORACLE` when it names an executable, else `erl` on
/// `$PATH`. `Err(reason)` means "no usable oracle" — the caller skips **loudly**.
fn resolve_oracle() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Ok(program);
        }
        return Err(format!(
            "$ERTS_ORACLE='{}' is not an executable file",
            program.display()
        ));
    }
    let path = std::env::var_os("PATH").ok_or("PATH is unset and $ERTS_ORACLE is unset")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err("$ERTS_ORACLE is unset and `erl` is not on $PATH".to_string())
}

/// The system `erlc`: next to the resolved `erl`, else on `$PATH`. `Err(reason)`
/// means "no usable compiler" — the caller skips **loudly**.
fn resolve_erlc(oracle: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = oracle.parent() {
        let sibling = dir.join("erlc");
        if is_executable(&sibling) {
            return Ok(sibling);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("erlc");
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "no `erlc` next to the oracle '{}' or on $PATH",
        oracle.display()
    ))
}

/// Runs `command`, waiting at most `limit` for it to exit.
///
/// A process that cannot be spawned yields its `io::Error`, so the caller skips
/// **loudly** when the toolchain is absent. A process that does not exit within
/// `limit` is killed and this **panics**: the oracle is a real `erl`/`erlc`, and an
/// invocation that never terminates must fail CI rather than hang it forever. Both
/// outputs here are tiny (well under a pipe buffer), so reaping the child before
/// draining its pipes cannot deadlock.
fn run_bounded(command: &mut Command, limit: Duration) -> std::io::Result<std::process::Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the bounded oracle command did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A fresh, unique directory under the system temp dir for the oracle cross-check.
fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "beam_loader_fun_name_{}_{nanos}",
        std::process::id()
    ))
}

// ---------------------------------------------------------------------------
// Oracle output
// ---------------------------------------------------------------------------

/// One parsed oracle line: the fun's `old index`, its `uniq`, and the **name** the
/// runtime answers for `fun_info(F, name)`.
struct OracleFun {
    index: u32,
    uniq: u32,
    name: String,
}

/// Parses the oracle's `index=<i> uniq=<u> name=<name>` lines (the name runs to the
/// end of the line, so it needs no escaping).
fn parse_oracle_funs(output: &str) -> Vec<OracleFun> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let index = field(line, "index=").parse().unwrap_or_else(|e| {
                panic!("the oracle's `index=` value of {line:?} is not an integer: {e}")
            });
            let uniq = field(line, "uniq=")
                .split_whitespace()
                .next()
                .expect("a non-empty field")
                .parse()
                .unwrap_or_else(|e| {
                    panic!("the oracle's `uniq=` value of {line:?} is not an integer: {e}")
                });
            let (_, name) = line
                .split_once(" name=")
                .unwrap_or_else(|| panic!("the oracle line {line:?} has no `name=` field"));
            OracleFun {
                index,
                uniq,
                name: name.trim().to_string(),
            }
        })
        .collect()
}

/// The whitespace-delimited value following `key` in `line`.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let (_, rest) = line
        .split_once(key)
        .unwrap_or_else(|| panic!("the oracle line {line:?} has no `{key}` field"));
    rest.split_whitespace().next().unwrap_or(rest)
}

// ---------------------------------------------------------------------------
// The cross-check
// ---------------------------------------------------------------------------

/// `FunDescriptor.function` is the `FunT` entry's function atom — the exact atom
/// `erlang:fun_info(F, name)` reports — for both a named local fun and an anonymous
/// lambda.
#[test]
fn fun_descriptor_function_is_the_funt_function_atom() {
    let oracle = match resolve_oracle() {
        Ok(program) => program,
        Err(reason) => {
            eprintln!("SKIP: {reason} — the `FunT` function-atom cross-check needs the system `erl`");
            return;
        }
    };
    let erlc = match resolve_erlc(&oracle) {
        Ok(program) => program,
        Err(reason) => {
            eprintln!("SKIP: {reason} — the `FunT` function-atom cross-check needs the system `erlc`");
            return;
        }
    };

    let dir = unique_temp_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("SKIP: cannot create {} for the oracle cross-check", dir.display());
        return;
    }
    let source = dir.join("fiprobe.erl");
    if let Err(error) = std::fs::write(&source, FIXTURE_SOURCE) {
        eprintln!("SKIP: cannot stage {} ({error})", source.display());
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    // Compile the scratch fixture with the system compiler.
    let mut compile = Command::new(&erlc);
    compile
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&source);
    match run_bounded(&mut compile, ORACLE_TIMEOUT) {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "SKIP: `{}` rejected the fixture source ({}): {}",
                erlc.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(error) => {
            eprintln!(
                "SKIP: cannot run `{}` ({error}) — need the system OTP compiler",
                erlc.display()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    let beam = dir.join("fiprobe.beam");
    let module = match load_file(&beam) {
        Ok(module) => module,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("the loader rejected the `erlc` image {}: {error:?}", beam.display());
        }
    };

    // Ask the oracle for each fixture fun's `name` item (plus the identity items
    // used to locate the matching descriptor).
    let eval = "Ns = [fiprobe:named(), fiprobe:anon(73)],\
                [io:format(\"index=~p uniq=~p name=~ts~n\",\
                    [element(2, erlang:fun_info(F, index)),\
                     element(2, erlang:fun_info(F, uniq)),\
                     atom_to_list(element(2, erlang:fun_info(F, name)))])\
                 || F <- Ns],\
                halt().";
    let mut erl = Command::new(&oracle);
    erl.args(["-noshell", "-pa"])
        .arg(&dir)
        .args(["-eval", eval]);
    let output = match run_bounded(&mut erl, ORACLE_TIMEOUT) {
        Ok(output) => output,
        Err(error) => {
            eprintln!(
                "SKIP: cannot run `{}` ({error}) — need the system OTP runtime",
                oracle.display()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "the oracle `erl` failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8(output.stdout).expect("the oracle writes UTF-8");
    let observed = parse_oracle_funs(&stdout);

    // The two fixture funs, in oracle order — never a vacuous comparison.
    let pinned: Vec<(u32, &str)> = PINNED.to_vec();
    assert_eq!(
        observed.len(),
        pinned.len(),
        "the oracle reported {} funs, expected {}:\n{stdout}",
        observed.len(),
        pinned.len()
    );
    println!("ORACLE: fiprobe funs {stdout}");

    for (position, (index, name)) in pinned.iter().enumerate() {
        let fun = &observed[position];
        assert_eq!(fun.index, *index, "unexpected oracle fun order:\n{stdout}");
        assert_eq!(fun.name, *name, "the oracle's `name` drifted:\n{stdout}");

        // Locate the descriptor by the module-unique `FunT` index, confirmed by
        // `uniq` — the name plays no part in the match.
        let matches: Vec<&FunDescriptor> = module
            .funs()
            .iter()
            .filter(|descriptor| descriptor.index == fun.index)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one FunT entry with index {}, found {}",
            fun.index,
            matches.len()
        );
        let descriptor = matches[0];
        assert_eq!(
            descriptor.uniq, fun.uniq,
            "the FunT entry at index {} is not the oracle's fun (uniq mismatch)",
            fun.index
        );

        // THE ASSERTION: the descriptor's function atom names the oracle's `name`.
        assert_eq!(
            module.atom_name(descriptor.function),
            Some(fun.name.as_str()),
            "FunDescriptor.function of the fun at index {} does not match \
             fun_info(F, name) = {:?}",
            fun.index,
            fun.name
        );
    }

    // Every `FunT` entry of the fixture is one of the two fixture funs (the file
    // emits exactly one lambda per source fun).
    assert_eq!(
        module.funs().len(),
        pinned.len(),
        "the fixture module should carry exactly {} `FunT` entries",
        pinned.len()
    );
}
