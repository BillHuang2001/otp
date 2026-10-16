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

//! `code:load_binary/3` (contract A.9) end to end, through the built `beam_rt` binary and
//! cross-checked against the live system Erlang.
//!
//! `code:load_binary(Mod, File, Bin)` loads a module from an **in-memory** `.beam` image and
//! records `File` verbatim as the module's filename. It is served from the runtime-native
//! floor (`src/native/code.rs`), which reproduces `lib/kernel/src/code.erl:667-688` clause for
//! clause:
//!
//! 1. the guard `is_atom(Mod), (is_list(File) orelse is_atom(File)), is_binary(Bin)` — a
//!    failure is the bare-atom `function_clause` raise (the oracle's own reason for
//!    `load_binary/3`, distinct from this floor's usual `badarg`);
//! 2. `modp(File)` — an atom is accepted; a **proper** list whose elements are not all
//!    integers is `{error,badarg}`; an **improper** list raises `function_clause`;
//! 3. `erlang:prepare_loading/2` — a well-formed image whose own module name is `Mod`, else
//!    `{error,badfile}` (an empty image, garbage, or another module's image);
//! 4. the code server's sticky check (after the image validates) ⇒
//!    `{error,sticky_directory}`;
//! 5. otherwise the image is registered under `Mod` and the answer is `{module,Mod}` — and
//!    `code:which/1` / `code:is_loaded/1` / `code:all_loaded/0` report the recorded filename.
//!
//! Every expected value below was verified against the system `erl` (`erl -noshell -eval '…'`,
//! OTP 29.1.1 / erts-17.1). The **guard-raise** cases are compared by the raised **reason
//! atom** only (the runtime's synthesized stacktrace is shorter than the oracle's and is not
//! part of the contract), while every value case is compared over the whole answer line.
//!
//! The user-module fixture is compiled **at test time** with the system `erlc` into a scratch
//! directory (never committed) and its image is read back with `file:read_file/1` under **both**
//! runtimes. Every test skips **loudly** — never silently passes — when the toolchain is
//! unavailable.

#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

/// The loud-skip banner (a missing toolchain must announce itself).
const BANNER: &str = "code_load_binary";

/// One finished probe: its exit code and its trimmed stdout / raw stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` through the shared bounded runner (a probe that blocks is killed and
/// reaped rather than hanging the suite) and collects its exit code and output.
fn run(program: &str, args: &[&str]) -> Run {
    let output = support::run_bounded(Path::new(program), args)
        .unwrap_or_else(|| panic!("`{program}` {args:?} could not be run"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The built runtime, running `expr` as its `-eval` program.
fn beam_rt(expr: &str) -> Run {
    run(env!("CARGO_BIN_EXE_beam_rt"), &["-noshell", "-eval", expr])
}

/// The **same** program under the oracle, terminated by `-s init stop` (an `erl -noshell`
/// never exits on its own).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    run(program, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

/// Both runtimes given the **same** `expr`, or `None` after a loud skip when no system `erl`
/// is available.
fn both(expr: &str) -> Option<(Run, Run)> {
    let oracle = support::require_oracle(BANNER)?;
    Some((beam_rt(expr), oracle_eval(&oracle, expr)))
}

/// Strips every whitespace character.
///
/// The `~p` printer wraps a wide term over several lines (a `line_length` of 80), and each
/// continuation line starts with an indent, so the answer's *layout* varies with the term's
/// width; the term's *content* is what the contract covers, so both sides are compared with
/// whitespace removed. No value under test contains meaningful whitespace inside a term.
fn norm(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Asserts both runtimes exited 0 with an empty stderr, this runtime printed exactly
/// `expected` (whitespace-normalized), and the oracle printed `expected` too.
///
/// The oracle's refusal paths **also log** (`beam_load.c`'s `corrupt file header` report and
/// its sticky-directory report) asynchronously on **stdout**; those reports are not part of
/// `code:load_binary/3`'s return value, so the oracle's answer is matched **within** its
/// stdout rather than as the whole of it.
fn assert_agrees((ours, theirs): (Run, Run), expr: &str, expected: &str) {
    assert_eq!(ours.code, 0, "beam_rt `{expr}`: {}", ours.err);
    assert!(
        ours.err.is_empty(),
        "beam_rt `{expr}` must be silent on stderr: {}",
        ours.err
    );
    assert_eq!(theirs.code, 0, "oracle `{expr}`: {}", theirs.err);
    assert_eq!(norm(&ours.out), norm(expected), "beam_rt `{expr}`");
    assert!(
        norm(&theirs.out).contains(&norm(expected)),
        "the oracle must print `{expected}` for `{expr}`; it printed:\n{}",
        theirs.out
    );
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// A valid-value program: prints the answer of `code:load_binary(Mod, File, B)` where `B` is
/// the image read from `beam_path` — and, right after it, the load-state observations.
fn expr_load(beam_path: &str, file: &str, module: &str, extras: &str) -> String {
    format!(
        "{{ok,B}} = file:read_file(\"{beam_path}\"), \
         io:format(\"~p~n\",[{{code:load_binary({module}, {file}, B){extras}}}]), halt(0)."
    )
}

/// A guard/`modp` program: prints the **reason atom** of the caught `function_clause` raise
/// (never the full stacktrace, which is not part of the contract), or the value when the call
/// returns one.
fn expr_caught(call: &str) -> String {
    format!(
        "io:format(\"~p~n\",[case catch {call} of \
         {{'EXIT',{{R,_}}}} -> R; Other -> Other end]), halt(0)."
    )
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Guard failures raise the bare atom `function_clause`, and a proper list whose elements are
/// not all integers is `{error,badarg}` — both matching the oracle.
#[test]
fn guard_failures_and_modp_agree_with_the_live_oracle() {
    // `is_atom(Mod)` fails.
    let expr = expr_caught("code:load_binary(42, \"f.beam\", <<>>)");
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "function_clause");

    // `is_list(File) orelse is_atom(File)` fails (an integer filename).
    let expr = expr_caught("code:load_binary(mymod, 123, <<>>)");
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "function_clause");

    // `is_binary(Bin)` fails.
    let expr = expr_caught("code:load_binary(mymod, \"f.beam\", not_a_binary)");
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "function_clause");

    // `modp/1`'s list arm: an improper list runs off the end of `int_list/1`.
    let expr = expr_caught("code:load_binary(mymod, [1|foo], <<>>)");
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "function_clause");

    // `modp/1` returns `false`: a proper list with a non-integer element ⇒ `{error,badarg}`.
    let expr = expr_caught("code:load_binary(mymod, [a,b], <<>>)");
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "{error,badarg}");
}

/// Every image that cannot be loaded is the oracle's exact `{error,badfile}` — an empty image,
/// garbage, an atom filename with a bad image, and an int-list filename with a bad image.
#[test]
fn unloadable_images_are_badfile_like_the_oracle() {
    for expr in [
        "io:format(\"~p~n\",[code:load_binary(mymod, \"/x/mymod.beam\", <<>>)]), halt(0).",
        "io:format(\"~p~n\",[code:load_binary(mymod, \"g.beam\", \
          <<\"garbage-not-a-beam\">>)]), halt(0).",
        "io:format(\"~p~n\",[code:load_binary(mymod, cover_compiled, <<>>)]), halt(0).",
        "io:format(\"~p~n\",[code:load_binary(mymod, [104,105], <<>>)]), halt(0).",
    ] {
        let Some(runs) = both(expr) else { return };
        assert_agrees(runs, expr, "{error,badfile}");
    }
}

/// A valid image loads (`{module, Mod}`), the module becomes callable, and the recorded
/// filename — a real path string — is reported by `which`/`is_loaded`/`all_loaded`; a second
/// load is idempotent; an image requested under a **different** name is `{error,badfile}`.
#[test]
fn a_valid_image_loads_and_records_its_filename() {
    let Some(fixture) = build_fixture() else {
        return;
    };
    let beam = fixture.beam().display().to_string();

    // A real-path filename: the filename is reported verbatim by all three readers.
    let expr = expr_load(
        &beam,
        "\"/tmp/tb_mod_loaded.beam\"",
        "tb_mod",
        ", code:which(tb_mod), code:is_loaded(tb_mod), tb_mod:hello(), \
         lists:keyfind(tb_mod,1,code:all_loaded())",
    );
    let Some(runs) = both(&expr) else { return };
    assert_agrees(
        runs,
        &expr,
        "{{module,tb_mod},\"/tmp/tb_mod_loaded.beam\",\
         {file,\"/tmp/tb_mod_loaded.beam\"},world,\
         {tb_mod,\"/tmp/tb_mod_loaded.beam\"}}",
    );

    // An **empty** filename (`[]`) is reported as `[]` — the memory-load shape.
    let expr = expr_load(
        &beam,
        "\"\"",
        "tb_mod",
        ", code:which(tb_mod), code:is_loaded(tb_mod), \
         lists:keyfind(tb_mod,1,code:all_loaded())",
    );
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "{{module,tb_mod},[],{file,[]},{tb_mod,[]}}");

    // An **atom** filename is reported as the atom.
    let expr = expr_load(
        &beam,
        "cover_compiled",
        "tb_mod",
        ", code:which(tb_mod), code:is_loaded(tb_mod)",
    );
    let Some(runs) = both(&expr) else { return };
    assert_agrees(
        runs,
        &expr,
        "{{module,tb_mod},cover_compiled,{file,cover_compiled}}",
    );

    // An image whose own module name differs from the requested `Mod` is `{error,badfile}`
    // (`expr_load` prints its answer inside a 1-tuple, hence the doubled braces).
    let expr = expr_load(&beam, "\"/tmp/other.beam\"", "other_mod", "");
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "{{error,badfile}}");

    // A second load is idempotent (`{module,Mod}` again).
    let expr = format!(
        "{{ok,B}} = file:read_file(\"{beam}\"), \
         io:format(\"~p~n\",[{{code:load_binary(tb_mod, \"\", B), \
         code:load_binary(tb_mod, \"\", B)}}]), halt(0)."
    );
    let Some(runs) = both(&expr) else { return };
    assert_agrees(runs, &expr, "{{module,tb_mod},{module,tb_mod}}");
}

/// A **sticky** target is refused with `{error,sticky_directory}` once its image validates,
/// while a bad image for the same sticky name is still `{error,badfile}` (the image is
/// prepared *before* the sticky check). `lists` resolves under the stdlib directory.
#[test]
fn a_sticky_target_is_refused_like_the_oracle() {
    let expr = "io:format(\"~p~n\",[code:load_binary(lists, \"\", \
                element(2,file:read_file(code:which(lists))))]), halt(0).";
    let Some(runs) = both(expr) else { return };
    assert_agrees(runs, expr, "{error,sticky_directory}");

    let expr = "io:format(\"~p~n\",[code:load_binary(lists, \"\", <<>>)]), halt(0).";
    let Some(runs) = both(expr) else { return };
    assert_agrees(runs, expr, "{error,badfile}");
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// The compiled fixture in one scratch directory (removed on drop).
///
/// It is deliberately **not** put on the code path: `code:which/1` after a memory load must
/// report the recorded filename, not a directory the fixture was found in.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// The compiled `.beam` both runtimes read back with `file:read_file/1`.
    fn beam(&self) -> PathBuf {
        self.dir.join("tb_mod.beam")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// The fixture source: a module that is neither preloaded nor under a sticky directory, kept
/// trivially small so the only thing under test is the load, not the loader's opcode coverage.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/code_load_binary.rs` (never committed).
-module(tb_mod).
-export([hello/0]).

hello() -> world.
"#;

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

/// Compiles the fixture into a fresh scratch directory, or `None` after a loud skip when the
/// toolchain cannot do it.
fn build_fixture() -> Option<Fixture> {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` on $PATH (and none beside the oracle)");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("beam_rt_code_load_binary_{}", std::process::id()));
    // A stale directory from a killed run must not decide the result: start clean.
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            support::skip_loudly(BANNER, &format!("cannot clear {}: {error}", dir.display()));
            return None;
        }
    }
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(BANNER, &format!("cannot create {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join("tb_mod.erl");
    if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
        support::skip_loudly(BANNER, &format!("cannot write {}: {error}", source.display()));
        return None;
    }
    let output = match Command::new(&erlc).arg("-o").arg(&dir).arg(&source).output() {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(BANNER, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            BANNER,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let fixture = Fixture { dir };
    if !fixture.beam().is_file() {
        support::skip_loudly(BANNER, "`erlc` produced no tb_mod.beam");
        return None;
    }
    Some(fixture)
}
