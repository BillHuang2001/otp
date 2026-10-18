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

//! Contract A.9 (**self-hosted build**) — the **bootstrap chain**: the compiler's own front end is
//! rebuilt, stage by stage, by a single running runtime, and each freshly built module is installed
//! over the loaded one **before** the next stage compiles with it.
//!
//! # The staged bootstrap, and what each stage proves
//!
//! One `beam_rt -noshell -pa <out> -eval '<chain>'` invocation (and one oracle invocation of the
//! identical chain, in that side's own directories) compiles four **real in-tree sources** with the
//! A.9 *E0* option list plus `{i, Dir}` for every `lib/*/include` directory in the tree, and installs
//! each artifact over the module the runtime is running on:
//!
//! | Stage | Source | What it proves |
//! |---|---|---|
//! | 1 | `lib/stdlib/src/epp.erl` | the preprocessor the whole chain depends on is built by this runtime, and **stage 2 compiles with the freshly built `epp`** |
//! | 2 | `lib/stdlib/src/erl_scan.erl` | the tokenizer is rebuilt; stages 3–6 lex with the fresh `erl_scan` |
//! | 3 | `lib/stdlib/src/erl_lint.erl` | the linter is rebuilt; stages 4–6 lint with the fresh `erl_lint` |
//! | 4 | `lib/compiler/src/compile.erl` | **the compiler itself** is rebuilt and installed — every later stage is compiled by the compiler this runtime just produced |
//! | 5 | a copy of `compile.erl` + a marker function | the **freshly loaded** `compile` is the code that runs: `compile:a9_selfhost_marker()` answers the marker value written by the test |
//! | 6 | `lib/stdlib/src/ordsets.erl` | the bootstrap **closes**: the compiler built/kitted in stages 4–5 compiles the next real module, and the Rust test byte-compares that artifact against the oracle's |
//!
//! # Why a *marker* stage exists (the byte-identity problem)
//!
//! Stage 1–4's artifacts are **byte-identical to the installed `.beam` files** (the same OTP sources
//! and the same `deterministic` option list, so the test asserts byte equality against the oracle's
//! own artifacts). Installing them therefore cannot change any observable behaviour, and a stage that
//! "worked" would look **exactly like** one that silently did nothing. Stage 5 is the non-vacuity
//! control: it compiles a copy of `compile.erl` carrying `-export([a9_selfhost_marker/0]).` and
//! `a9_selfhost_marker() -> a9_selfhost_marker_v1.`, installs it, and **calls it** — only the freshly
//! built module defines that function, so the answer proves the code that runs is the code this
//! invocation produced (`undef` would be the answer had the install not happened).
//!
//! # The install route (why `--pa` + `erlang:delete_module/1` + `code:load_binary/3`)
//!
//! A plain `code:load_binary(compile, Path, Bytes)` is **refused** — `{error,sticky_directory}` — on
//! *both* runtimes, because all four modules are already loaded from a sticky directory (kernel,
//! stdlib and compiler are `code.erl`'s `maybe_stick_dirs/1`). The oracle gets past that with
//! `code:unstick_mod/1`; this runtime's `code:*` surface is a native floor whose sticky check does not
//! consult the unstick state, so the chain instead uses the route **both** runtimes share: the fresh
//! artifact directory is put on the code path (`-pa`, before `-eval`) so the module name resolves to
//! this invocation's own non-sticky artifact, then `code:purge/1` + `erlang:delete_module/1` drop the
//! loaded module (OTP refuses to delete a module that still holds old code, hence the purge — its
//! return value is deliberately not recorded, because the floor always answers `false` while the
//! oracle answers whether a process was killed), and `code:load_binary/3` installs the fresh image and
//! records the fresh path, so `code:which/1` names this side's own artifact (asserted as a
//! **boolean** per stage, since the path itself differs between the two sides).
//!
//! Every stage records the same five fields — the compile result, that the artifact was written and is
//! non-empty, the delete, the load and "`code:which/1` is the fresh path" — and the whole chain prints
//! them as **one** marked line. The Rust side asserts that value field by field (see [`EXPECTED`]),
//! then that the live system `erl` answers the same value for the identical chain.
//!
//! # Measured cost
//!
//! Measured on this host (debug `beam_rt`, shared machine): **532 s** for the whole invocation
//! (`erl_lint.erl` ≈ 239 s, `epp.erl` ≈ 89 s, `compile.erl` ≈ 60 s, the marker build ≈ 60 s — the
//! same module again — `erl_scan.erl` ≈ 90 s, `ordsets.erl` ≈ 2 s) against **≈4 s** for the oracle's
//! own run of the identical chain. The bound is therefore [`PROBE_DEADLINE`] (30 min), not the shared
//! harness's 30 s [`support::PROBE_TIMEOUT`]: the probe is a genuine in-process compile of ~3 MB of
//! Erlang source under the debug interpreter. A missing toolchain, or a checkout without the sources,
//! is a **loud skip** — never a silent pass.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery; the
// unused remainder is expected here — the same shape `tests/selfhost_chain.rs` uses.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The longest this chain may run before it is killed and reaped: the measured cost is ≈532 s on a
/// loaded host (see the module docs), a generous multiple of it rather than the shared harness's 30 s
/// probe bound — this probe is a genuine in-process compile, not a query.
const PROBE_DEADLINE: Duration = Duration::from_secs(1800);

/// How often a running probe's exit is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The marker pair the chain prints its values between.
const MARK: &str = "A9S|";
const END: &str = "|A9SEND";

// ---------------------------------------------------------------------------------------
// The source tree
// ---------------------------------------------------------------------------------------

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`) — the same discovery the in-crate
/// `src/context/pump.rs` sweep uses.
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// The four real sources the chain compiles, or `None` after a loud skip.
///
/// `erl_parse.erl` is deliberately **not** in the chain: it does not exist in the tree (it is
/// generated from `erl_parse.yrl`), so it cannot be the subject of a source-level bootstrap stage.
fn stage_sources(root: &Path, banner: &str) -> Option<[PathBuf; 4]> {
    let sources = [
        root.join("lib/stdlib/src/epp.erl"),
        root.join("lib/stdlib/src/erl_scan.erl"),
        root.join("lib/stdlib/src/erl_lint.erl"),
        root.join("lib/compiler/src/compile.erl"),
    ];
    for source in &sources {
        if !source.is_file() {
            support::skip_loudly(
                banner,
                &format!("no real source to compile at {}", source.display()),
            );
            return None;
        }
    }
    Some(sources)
}

/// The oracle module the final stage compiles: a plain stdlib source, so the artifact proves the
/// freshly built compiler works on a module none of the earlier stages touched.
fn final_source(root: &Path) -> PathBuf {
    root.join("lib/stdlib/src/ordsets.erl")
}

/// Every `lib/*/include` directory in the tree, sorted.
///
/// These are the `{i, Dir}` entries of the option list: `compile.erl` includes
/// `erl_compile.hrl` (which lives in `lib/stdlib/include`, not beside the source) and the other
/// sources include their own headers from `lib/*/include` too. The list is discovered generically —
/// no application name is hardcoded — so a tree with a different set of applications still gets a
/// complete include path.
fn include_dirs(root: &Path) -> Vec<PathBuf> {
    let lib = root.join("lib");
    let mut dirs: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&lib) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let include = entry.path().join("include");
        if include.is_dir() {
            dirs.push(include);
        }
    }
    dirs.sort();
    dirs
}

// ---------------------------------------------------------------------------------------
// Running the two runtimes (bounded by PROBE_DEADLINE, not the harness's 30 s)
// ---------------------------------------------------------------------------------------

/// Runs `program` with `args`, stdin at `/dev/null`, output captured, **polled** against
/// [`PROBE_DEADLINE`] so a chain that blocks is killed and reaped instead of hanging the suite.
fn run_chain(program: &Path, args: &[&str], what: &str) -> Output {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}` for {what}: {error}", program.display()));
    let deadline = Instant::now() + PROBE_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("cannot collect {what}: {error}"))
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "{what}: `{}` outlived the {}s chain bound and was killed",
                        program.display(),
                        PROBE_DEADLINE.as_secs()
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("cannot poll {what}: {error}");
            }
        }
    }
}

/// `<beam_rt> -noshell -pa out -eval expr` — the runtime under test, on the chain's own route.
fn beam_rt_chain(out: &Path, expr: &str) -> Output {
    run_chain(
        Path::new(BINARY),
        &[
            "-noshell",
            "-pa",
            out.to_str().expect("an ASCII scratch path"),
            "-eval",
            expr,
        ],
        "beam_rt",
    )
}

/// The oracle's own run of the identical chain, with its own fresh artifact directory on the code
/// path **before** `-eval` (the oracle evaluates in command-line order).
fn oracle_chain(oracle: &Path, out: &Path, expr: &str) -> Output {
    run_chain(
        oracle,
        &[
            "-noshell",
            "-pa",
            out.to_str().expect("an ASCII scratch path"),
            "-eval",
            expr,
            "-s",
            "init",
            "stop",
        ],
        "the system erl",
    )
}

// ---------------------------------------------------------------------------------------
// Marked values
// ---------------------------------------------------------------------------------------

/// The probe's own line, extracted from `stdout` between `start` and `end`.
///
/// `~p` wraps a term longer than the line width onto continuation lines, so the *marker pair* — not
/// the whole stdout — delimits the value, and the wrap's only effect (leading indentation) is undone
/// by trimming each continuation line and joining them back. That is the `unwrapped/1` rule the
/// sibling suites use, and it is safe because **no field compared here contains a space** (`~p` on an
/// atom, tuple, list of those or small integer prints no whitespace).
fn marked(program: &str, stdout: &str, start: &str, end: &str) -> String {
    let Some(from) = stdout.find(start) else {
        panic!("`{program}` printed no `{start}` marker; stdout:\n{stdout}");
    };
    let rest = &stdout[from + start.len()..];
    let Some(upto) = rest.find(end) else {
        panic!("`{program}` printed no `{end}` terminator; stdout:\n{stdout}");
    };
    rest[..upto].lines().map(str::trim_start).collect()
}

/// Asserts the run exited `0` and prints the marked value.
fn value_of(program: &str, output: &Output, start: &str, end: &str, what: &str) -> String {
    let code = output.status.code().unwrap_or(-1);
    assert_eq!(
        code,
        0,
        "{program}: {what}: the run must exit 0 — stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    marked(program, &String::from_utf8_lossy(&output.stdout), start, end)
}

// ---------------------------------------------------------------------------------------
// Scratch directories and Erlang literals
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_selfhost_bootstrap_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_selfhost_bootstrap_{}_{unique}_{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }

    /// The artifact `module.beam` inside the directory.
    fn beam(&self, module: &str) -> PathBuf {
        self.0.join(format!("{module}.beam"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The **Erlang string literal** for `path`: the scratch names this suite builds are ASCII, which
/// Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII scratch path"))
}

/// The A.9 *E0* option list for `outdir`, plus one `{i, Dir}` per include directory in the tree
/// (`extra` adds a further include directory, used by the marker stage — its copy lives outside the
/// tree, so `epp` needs `lib/compiler/src` to find `core_parse.hrl`).
fn options(outdir: &Path, includes: &[PathBuf], extra: Option<&Path>) -> String {
    let mut list = format!(
        "[no_spawn_compiler_process, deterministic, no_docs, {{outdir, {}}}",
        literal(outdir)
    );
    for dir in includes.iter().map(PathBuf::as_path).chain(extra) {
        list.push_str(&format!(", {{i, {}}}", literal(dir)));
    }
    list.push(']');
    list
}

/// The marker source: a copy of `compile.erl` with `-export([a9_selfhost_marker/0]).` immediately
/// after its `-module` attribute and the marker function appended.
///
/// It keeps the module name `compile` **and** the file base name `compile.erl`, because `compile:file`
/// checks that the two agree (`{module_name, …}` otherwise) — so the copy is written as
/// `<scratch>/compile.erl`, outside the tree, and its artifact lands in the marker stage's own output
/// directory.
fn marker_source(root: &Path) -> String {
    let original = std::fs::read_to_string(root.join("lib/compiler/src/compile.erl"))
        .expect("the compiler source is readable");
    let needle = "-module(compile).\n";
    assert_eq!(
        original.matches(needle).count(),
        1,
        "`compile.erl` must declare its module exactly once"
    );
    let with_export = original.replacen(
        needle,
        "-module(compile).\n-export([a9_selfhost_marker/0]).\n",
        1,
    );
    with_export + "\na9_selfhost_marker() -> a9_selfhost_marker_v1.\n"
}

// ---------------------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------------------

/// The expected marked value, field by field — re-verified against the system `erl` (OTP 29.1.1),
/// which answers the identical chain with the identical value.
///
/// ```text
///  epp:       {ok,epp}      the preprocessor compiled
///             true           its artifact was written and is non-empty
///             true           erlang:delete_module(epp)
///             {module,epp}   code:load_binary/3 installed it
///             true           code:which(epp) is this side's fresh artifact
///  erl_scan:  … same five …
///  erl_lint:  … same five …
///  compile:   … same five — the compiler itself, rebuilt and installed …
///  marker:    {ok,compile}  the modified copy compiled
///             true           artifact written, non-empty
///             true           delete (after code:purge/1)
///             {module,compile} installed over the just-built compiler
///             true           code:which(compile) is the marker artifact
///  a9_selfhost_marker_v1     **the freshly built compiler is the code that runs**
///  {ok,ordsets}             the bootstrapped compiler compiles the next real module
///             true           that artifact was written and is non-empty
/// ```
const EXPECTED: &str = "[{ok,epp},true,true,{module,epp},true,\
                         {ok,erl_scan},true,true,{module,erl_scan},true,\
                         {ok,erl_lint},true,true,{module,erl_lint},true,\
                         {ok,compile},true,true,{module,compile},true,\
                         {ok,compile},true,true,{module,compile},true,\
                         a9_selfhost_marker_v1,{ok,ordsets},true]";

/// One stage of the chain: `(label, module, source, options, artifact dir)`.
struct Stage<'a> {
    label: &'a str,
    module: &'a str,
    source: PathBuf,
    options: String,
    artifact: PathBuf,
}

/// Builds the chain expression for one side's own directories.
fn chain(stages: &[Stage<'_>], final_source: &Path, final_options: &str, out: &Path) -> String {
    let mut expr = String::new();
    let mut fields: Vec<String> = Vec::new();
    for stage in stages {
        let l = stage.label;
        let m = stage.module;
        expr.push_str(&format!(
            "C_{l} = compile:file({src}, {opts}),\n\
             P_{l} = {path},\n\
             B_{l} = case file:read_file(P_{l}) of {{ok, X_{l}}} -> X_{l}; _ -> empty end,\n\
             Z_{l} = case B_{l} of empty -> false; _ -> (byte_size(B_{l}) > 0) end,\n\
             PU_{l} = code:purge({m}),\n\
             D_{l} = erlang:delete_module({m}),\n\
             L_{l} = code:load_binary({m}, P_{l}, B_{l}),\n\
             W_{l} = (code:which({m}) =:= P_{l}),\n",
            src = literal(&stage.source),
            opts = stage.options,
            path = literal(&stage.artifact),
        ));
        fields.push(format!("C_{l},Z_{l},D_{l},L_{l},W_{l}"));
    }
    expr.push_str(&format!(
        "MK = compile:a9_selfhost_marker(),\n\
         C_fin = compile:file({src}, {opts}),\n\
         P_fin = {path},\n\
         B_fin = case file:read_file(P_fin) of {{ok, X_fin}} -> X_fin; _ -> empty end,\n\
         Z_fin = case B_fin of empty -> false; _ -> (byte_size(B_fin) > 0) end,\n",
        src = literal(final_source),
        opts = final_options,
        path = literal(&out.join("ordsets.beam")),
    ));
    fields.push("MK,C_fin,Z_fin".to_string());
    expr.push_str(&format!(
        "io:format(\"{mark}~p{end}~n\", [[{fields}]]),\nhalt(0).",
        mark = MARK,
        end = END,
        fields = fields.join(","),
    ));
    expr
}

/// Asserts a stage's artifact exists, is non-empty, and is **byte-identical** to the oracle's own
/// artifact for the same source and option list.
fn assert_artifact(dir: &Path, oracle_dir: &Path, module: &str, what: &str) -> u64 {
    let read = |root: &Path, who: &str| {
        let path = root.join(format!("{module}.beam"));
        let bytes = std::fs::read(&path).unwrap_or_else(|error| {
            panic!("{what}: {who} reported success but wrote no {}: {error}", path.display())
        });
        assert!(!bytes.is_empty(), "{what}: {who}'s {} is empty", path.display());
        bytes
    };
    let mine = read(dir, "beam_rt");
    let theirs = read(oracle_dir, "the system erl");
    assert_eq!(
        mine, theirs,
        "{what}: beam_rt's `{module}.beam` ({} bytes) must be byte-identical to the oracle's \
         ({} bytes) for the same source and the same option list",
        mine.len(),
        theirs.len()
    );
    mine.len() as u64
}

/// **The A.9 bootstrap chain.** One `beam_rt` invocation rebuilds `epp`, `erl_scan`, `erl_lint` and
/// `compile` from the real in-tree sources, installs each fresh artifact over the loaded module,
/// proves with a marker function that the freshly loaded compiler is the code that runs, and then
/// has that compiler compile `lib/stdlib/src/ordsets.erl`.
///
/// The live system `erl` runs the identical chain in its own directories and must answer the same
/// marked value ([`EXPECTED`]); every artifact both sides produce is then byte-compared, so the
/// "same value" is backed by "same object code".
#[test]
fn the_compiler_rebuilds_itself_stage_by_stage_and_the_fresh_modules_are_what_runs() {
    const BANNER: &str = "selfhost_bootstrap";
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let root = source_root();
    let Some(sources) = stage_sources(&root, BANNER) else {
        return;
    };
    let final_source = final_source(&root);
    if !final_source.is_file() {
        support::skip_loudly(
            BANNER,
            &format!("no real source to compile at {}", final_source.display()),
        );
        return;
    }
    let includes = include_dirs(&root);
    assert!(
        !includes.is_empty(),
        "{BANNER}: the tree must carry at least one `lib/*/include` directory"
    );

    // Each side compiles into its own artifact directories (so `code:which/1` is a property of that
    // side's own path) and the marker stage gets a directory of its own — stage 4's `compile.beam`
    // must survive for the byte comparison.
    let rust_out = TempDir::new("rust_out");
    let rust_seed = TempDir::new("rust_seed");
    let oracle_out = TempDir::new("oracle_out");
    let oracle_seed = TempDir::new("oracle_seed");

    // The marker source is written **once** and compiled by both runtimes, so the two artifacts are
    // built from identical bytes.
    let scratch = TempDir::new("marker");
    let marker = scratch.0.join("compile.erl");
    std::fs::write(&marker, marker_source(&root)).expect("write the marker source");

    let compiler_src = root.join("lib/compiler/src");
    let build = |out: &TempDir, seed: &TempDir| {
        let out_options = options(&out.0, &includes, None);
        let seed_options = options(&seed.0, &includes, Some(&compiler_src));
        let stages = vec![
            Stage {
                label: "epp",
                module: "epp",
                source: sources[0].clone(),
                options: out_options.clone(),
                artifact: out.beam("epp"),
            },
            Stage {
                label: "scan",
                module: "erl_scan",
                source: sources[1].clone(),
                options: out_options.clone(),
                artifact: out.beam("erl_scan"),
            },
            Stage {
                label: "lint",
                module: "erl_lint",
                source: sources[2].clone(),
                options: out_options.clone(),
                artifact: out.beam("erl_lint"),
            },
            Stage {
                label: "comp",
                module: "compile",
                source: sources[3].clone(),
                options: out_options.clone(),
                artifact: out.beam("compile"),
            },
            Stage {
                label: "mark",
                module: "compile",
                source: marker.clone(),
                options: seed_options,
                artifact: seed.beam("compile"),
            },
        ];
        chain(&stages, &final_source, &out_options, &out.0)
    };

    // ---- the runtime under test ----
    let started = Instant::now();
    let ours_output = beam_rt_chain(&rust_out.0, &build(&rust_out, &rust_seed));
    let ours_elapsed = started.elapsed();
    let ours = value_of("beam_rt", &ours_output, MARK, END, "the bootstrap chain");
    assert!(
        ours_output.stderr.is_empty(),
        "beam_rt: a successful chain must be silent on stderr — {}",
        String::from_utf8_lossy(&ours_output.stderr)
    );

    // ---- the live oracle, same chain, its own directories ----
    let started = Instant::now();
    let theirs_output = oracle_chain(&oracle, &oracle_out.0, &build(&oracle_out, &oracle_seed));
    let theirs_elapsed = started.elapsed();
    let theirs = marked(
        "erl",
        &String::from_utf8_lossy(&theirs_output.stdout),
        MARK,
        END,
    );

    // Assert the runtime's own value first, then oracle agreement (never weaken the order).
    assert_eq!(ours, EXPECTED, "beam_rt: the bootstrap chain (marked value {ours:?})");
    assert_eq!(
        ours,
        theirs,
        "the system erl ({}) must answer the same value for the bootstrap chain; it answered \
         {theirs:?}",
        oracle.display()
    );

    // ---- the artifacts: every stage's object code, byte-identical to the oracle's ----
    let epp = assert_artifact(&rust_out.0, &oracle_out.0, "epp", "(1) epp");
    let scan = assert_artifact(&rust_out.0, &oracle_out.0, "erl_scan", "(2) erl_scan");
    let lint = assert_artifact(&rust_out.0, &oracle_out.0, "erl_lint", "(3) erl_lint");
    let compiler = assert_artifact(&rust_out.0, &oracle_out.0, "compile", "(4) compile");
    let marker_bytes = assert_artifact(&rust_seed.0, &oracle_seed.0, "compile", "(5) marker");
    let ordsets = assert_artifact(&rust_out.0, &oracle_out.0, "ordsets", "(6) ordsets");

    eprintln!(
        "selfhost_bootstrap: one beam_rt invocation ({:.1} s; the oracle's own run of the identical \
         chain {:.1} s) rebuilt epp ({epp} B) → erl_scan ({scan} B) → erl_lint ({lint} B) → compile \
         ({compiler} B) → a marker build ({marker_bytes} B), each installed over the loaded module, \
         the marker build answered `a9_selfhost_marker_v1`, and that compiler produced ordsets \
         ({ordsets} B) — every artifact byte-identical to the oracle's",
        ours_elapsed.as_secs_f64(),
        theirs_elapsed.as_secs_f64(),
    );
}
