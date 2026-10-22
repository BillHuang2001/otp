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
//! **The in-process `compile:file/2` route answers the same at every staging path length** —
//! the regression pin for the address-layout-sensitive abort that made it a coin flip.
//!
//! # The bug this pins
//!
//! The differential case `selfhost/compile_file_of_a_module_whose_moduledoc_lives_in_a_file`
//! (contract A.9) compiled a module **in-process** — `compile:file/2` with
//! `no_spawn_compiler_process` — and sometimes died with exit 134 and
//! `panicked at crates/beam_term/src/ops.rs:194:26: class: invalid boxed header subtag 0x3a`.
//! The trigger was the **absolute path length** of the directory the case was staged in: the
//! compile re-runs the engine inside itself (a cross-module callee's second machine, a fun the
//! engine applied), and every **nested** run reached `src/context/gc.rs`'s
//! `alloc_words_rooted` unarmed arm, which served its block from the pointer-stable,
//! **unscanned** arena even though the running process's scanned young heap was live. A
//! container built there (the compiler's literal-table map key cons,
//! `lib/compiler/src/beam_dict.erl`'s `literal/2`) was invisible to the collector, so a later
//! collection relocated the young-heap children it held and it kept a stale word — which
//! surfaced as a recycled carrier's leftover bytes at a later map **key** compare. The stale
//! word's value is whatever that address happened to hold, i.e. it depends on the process's
//! address layout, i.e. on the staged path's length. The fix makes the unarmed arm serve the
//! process's scanned young heap (arena only on a refusal).
//!
//! The *invariant* — a nested run's container is built on the process's scanned young heap —
//! is pinned profile-independently by `tests/nested_run_allocation_heap.rs`. **This** suite is
//! the end-to-end evidence: the real route, the real compiler, the real abort.
//!
//! # What the test does, and why the spread
//!
//! The module is `SELFHOST_DOCFILE_SOURCE` — a verbatim copy of the corpus constant
//! `tools/differential/src/corpus/selfhost.rs`'s `SELFHOST_DOCFILE_SOURCE` (the harness owns
//! that text; this suite may not depend on the sibling crate, so the copy is kept byte-identical
//! and this note is the thing to check when it changes). Its `-moduledoc({file, "moduledoc.txt"})`
//! forces `epp` to read a sibling file through the file server *while* the module compiles
//! itself, and `compile_self/0` answers `1` iff the produced object code's `Docs` chunk carries
//! exactly the text it staged — so the observable is the doc content, never "it didn't crash".
//!
//! The suite stages that module in a scratch directory whose **absolute path length it
//! controls** (the directory name is padded) and runs
//! `<beam_rt> run <dir>/selfhost_docfile.beam selfhost_docfile:compile_self/0` as a
//! **subprocess**, once per length, then requires the same answer at every one: exit `0`, empty
//! stderr and byte-identical stdout (`1\n`).
//!
//! The 18 lengths are the **absolute** staging-directory length, chosen for two reasons:
//!
//! * **The known band.** `58` — the length the bug was originally reported at — with its
//!   neighbours `56`, `59` and `61..=64`, plus `48` and `68..=72`, which are the lengths that
//!   abort deterministically on the pre-fix code in this sandbox under the dev profile (the
//!   whole `45..=130` band was swept to find them; `58` itself does not abort on every host,
//!   which is exactly why the invariant has its own test).
//! * **Size-class crossings.** `52`, `80`, `96`, `112`, `128` and `192` sit at, or just past,
//!   the allocator's larger size-class boundaries (the 16-byte-quantised power-of-two steps a
//!   path string is served from), so a defect that only shows when the staged path is served
//!   from a *different* size class than the poisoned one is still caught.
//!
//! # Failing on the pre-fix code
//!
//! With `alloc_words_rooted`'s unarmed arm back at `return runtime.alloc_words(n)` the suite
//! fails at the **first** offending length — measured on this sandbox in the dev profile, 4 of
//! 4 runs: absolute length `48` (the suite's own `<pid>_stage` name shape), stderr
//! `panicked at crates/beam_term/src/ops.rs:194:26: class: invalid boxed header subtag 0x2`,
//! exit `134`, empty stdout. The offending *set* is a property of the host's address layout
//! (the pid's own digits are part of the staged name), which is why the band is dense rather
//! than a single probe: a sweep of `45..=130` with a differently shaped name aborted at `60`
//! and `68..=72` instead. The suite therefore states the end-to-end contract, and
//! `nested_run_allocation_heap.rs` states the rule that no layout difference can hide.
//!
//! # Toolchain
//!
//! The fixture `.beam` is produced **once per run** with the system `erlc` (there is no
//! committed image of this module; it is compiled from source by design, because the suite's
//! whole point is an in-process recompile of that very source). A missing `erl`/`erlc` is a
//! **loud skip** (`support::require_oracle` / [`skip_loudly`]), never a silent pass. Each length
//! gets its own directory re-seeded from those bytes, because the runtime may truncate or
//! remove the staged `.beam` when a compile fails — a failure at one length must not cascade.
#[allow(dead_code)]
mod support;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
/// The built runtime under test (this crate's bin target), exactly as the sibling contract
/// suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The fixture module's name: its source stem, its `.beam` stem and the module the `run` action
/// names.
const MODULE: &str = "selfhost_docfile";
/// The compiled images the runtime is pointed at, per length.
const BEAM_FILE: &str = "selfhost_docfile.beam";
/// The source `compile_self/0` recompiles in-process.
const SOURCE_FILE: &str = "selfhost_docfile.erl";
/// The doc file `-moduledoc({file, "moduledoc.txt"})` makes `epp` read. `compile_self/0` writes
/// its real content itself; a one-byte placeholder is staged with the module (the shape the
/// original reproducer required) so the read path is exercised from the first byte.
const DOC_FILE: &str = "moduledoc.txt";
/// The fixture source — **verbatim** `tools/differential/src/corpus/selfhost.rs`'s
/// `SELFHOST_DOCFILE_SOURCE` (see the module docs: the sibling crate is not a dependency of this
/// one, so the text is copied and must be kept in sync by hand).
const SELFHOST_DOCFILE_SOURCE: &str = r#"-module(selfhost_docfile).
-moduledoc({file, "moduledoc.txt"}).
-export([compile_self/0]).

%% A.9: this module's documentation lives in a SIBLING FILE, so `epp` reads it through the
%% file server (`file:read_file_info/1` + `file:read/2`, the file_io_service
%% `read_handle_info` request) while `compile:file/2` parses and compiles this very module
%% IN-PROCESS. `1` iff the produced object code's `Docs` chunk carries exactly the text this
%% run staged into that file, `2` otherwise.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Source = filename:join(Dir, "selfhost_docfile.erl"),
    DocFile = filename:join(Dir, "moduledoc.txt"),
    Doc = list_to_binary(doc_text()),
    ok = file:write_file(DocFile, Doc),
    {ok, ?MODULE} = compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}]),
    Beam = filename:join(Dir, "selfhost_docfile.beam"),
    case module_doc(Beam) =:= Doc of
        true -> 1;
        false -> 2
    end.

%% The module documentation this run stages into the sibling file. Plain ASCII, and no leading
%% or trailing whitespace, so the compiler's `string:trim/1` normalisation is an identity on it
%% and the `Docs` chunk's module doc is byte-identical to this file's content.
doc_text() ->
    "This module's documentation lives in a sibling file.\n"
    "\n"
    "The -moduledoc({file, ...}) attribute makes epp read that file through the file\n"
    "server, so the compiler's Docs chunk carries the text this run staged.".

%% The module doc the produced object code carries, read back from its `Docs` chunk. The chunk
%% is a compressed `docs_v1` term whose module-doc field is a bare binary before OTP 27 and a
%% language-keyed map from OTP 27 on, so both shapes are accepted: the golden is the text
%% equality, never a doc-format detail of the oracle's release.
module_doc(Beam) ->
    {ok, {?MODULE, [{"Docs", Docs}]}} = beam_lib:chunks(Beam, ["Docs"]),
    case binary_to_term(Docs) of
        {docs_v1, _Anno, _Mod, _Format, Doc, _Meta, _Docs} when is_binary(Doc) ->
            Doc;
        {docs_v1, _Anno, _Mod, _Format, Doc, _Meta, _Docs} when is_map(Doc) ->
            maps:get(<<"en">>, Doc, <<>>)
    end.
"#;
/// The one correct answer, on the fixed runtime and on the oracle alike: `compile_self/0`
/// answers `1`, and the `run` action prints it with `~w~n`.
const EXPECTED_STDOUT: &[u8] = b"1\n";
/// The **absolute** staging-directory lengths this suite requires the same answer at — the band
/// around the originally-crashing `58` (see the module docs) plus the allocator size-class
/// crossings.
const LENGTHS: [usize; 18] = [
    48, 52, 56, 58, 59, 60, 61, 62, 63, 64, 70, 71, 72, 80, 96, 112, 128, 192,
];
/// The minimum number of the [`LENGTHS`] probes that must be stageable on a host for the sweep to
/// be meaningful (a `$TMPDIR` so long that shorter absolute lengths cannot be built under it
/// makes the case un-runnable — a loud skip, never a silent pass).
const MIN_STAGED_LENGTHS: usize = 8;
/// How many bytes of a child's stderr a failure message carries (the abort's own text is at the
/// front; the backtrace that follows is truncated).
const STDERR_EXCERPT: usize = 600;
/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
///
/// The shared [`support`] module resolves the *oracle* only (this suite needs the compiler too),
/// so the one extra resolver lives here rather than being duplicated into the shared module
/// every suite compiles — the same shape `tests/gc_run.rs` and `tests/structured_reason_lifetime.rs`
/// use.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}
/// Prints the loud skip banner — a missing toolchain is never a silent pass.
fn skip_loudly(why: &str) {
    support::skip_loudly("path length independence", why);
}
/// A private scratch directory, removed when dropped.
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        // The directory is ours and disposable; a cleanup failure is reported, not hidden.
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "warning: could not remove the scratch dir {}: {error}",
                self.0.display()
            );
        }
    }
}
/// A fresh, empty scratch directory named `<label>_<pid>_<n>`.
fn scratch(label: &str) -> Scratch {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "beam_rt_path_length_{label}_{}_{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("a private scratch directory");
    Scratch(path)
}
/// A scratch directory whose **absolute** path is exactly `length` bytes, or `None` when the
/// system temp directory leaves no room for that length (a loud note is printed then: the
/// length cannot be staged on this host, which is not a failure of the runtime).
///
/// The name is `<pid>_<tag>` padded with `x`, so two lengths can never collide, and the actual
/// length is asserted after the join (a `$TMPDIR` with a trailing separator changes the pad by
/// one, so the pad is corrected by the observed delta rather than assumed).
fn staged_dir(length: usize, tag: &str) -> Option<Scratch> {
    let base = std::env::temp_dir();
    let base_len = base.to_str()?.len();
    let prefix = format!("b{}_{tag}", std::process::id());
    let pad = length.checked_sub(base_len + 1)?;
    if pad < prefix.len() + 1 {
        eprintln!(
            "note: cannot stage an absolute path of length {length} under {} (length {base_len}): \
             the padded name would be shorter than its own prefix",
            base.display()
        );
        return None;
    }
    let mut name = prefix;
    name.push_str(&"x".repeat(pad - name.len()));
    let mut dir = base.join(&name);
    let actual = dir.to_str()?.len();
    if actual != length {
        // Correct the pad by the observed delta (a separator `temp_dir()` already ends with).
        let delta = length as isize - actual as isize;
        let fixed = (pad as isize + delta).max(1) as usize;
        let mut name = format!("b{}_{tag}", std::process::id());
        if fixed <= name.len() {
            return None;
        }
        name.push_str(&"x".repeat(fixed - name.len()));
        dir = base.join(&name);
    }
    let actual = dir.to_str()?.len();
    if actual != length {
        eprintln!(
            "note: could not stage an absolute path of length {length} under {} (got {actual})",
            base.display()
        );
        return None;
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).ok()?;
    Some(Scratch(dir))
}
/// Compiles [`SELFHOST_DOCFILE_SOURCE`] once with `erlc` in its own scratch dir and returns the
/// `.beam` bytes plus the dir that owns them.
///
/// A compiler that is present and fails is a genuine tool failure (a loud panic carrying its
/// stderr), never a skip — the same rule `tools/differential/src/module_build.rs` states.
fn build_fixture(erlc: &Path) -> (Vec<u8>, Scratch) {
    let dir = scratch("build");
    let source = dir.0.join(SOURCE_FILE);
    fs::write(&source, SELFHOST_DOCFILE_SOURCE).expect("the fixture source is written");
    let out = dir.0.to_str().expect("the scratch path is UTF-8");
    let source = source.to_str().expect("the scratch path is UTF-8");
    let output = support::run_bounded_with_env(erlc, &["-o", out, source], &[]).unwrap_or_else(|| {
        panic!(
            "`{}` did not finish compiling the fixture within {}s",
            erlc.display(),
            support::PROBE_TIMEOUT.as_secs()
        )
    });
    assert!(
        output.status.success(),
        "`erlc` failed to compile the fixture (exit {:?}):\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let beam = dir.0.join(BEAM_FILE);
    let bytes = fs::read(&beam).unwrap_or_else(|error| {
        panic!(
            "`erlc` reported success but {} is unreadable: {error}",
            beam.display()
        )
    });
    assert!(
        !bytes.is_empty(),
        "the compiled fixture {} is empty",
        beam.display()
    );
    (bytes, dir)
}
/// "the first `STDERR_EXCERPT` bytes of `stderr`, newlines escaped" — for a failure message.
fn excerpt(stderr: &[u8]) -> String {
    let taken = &stderr[..stderr.len().min(STDERR_EXCERPT)];
    let mut text = String::from_utf8_lossy(taken).replace('\n', " | ");
    if stderr.len() > STDERR_EXCERPT {
        text.push_str(" …");
    }
    text
}
// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------
/// **The real in-process compile route must answer `1` at every staged absolute path length.**
///
/// See the module docs: 18 lengths spanning the originally-crashing band (including `58`, its
/// neighbours and the lengths that abort deterministically on the pre-fix code here) and the
/// allocator's larger size-class crossings; each run is a fresh `beam_rt run` subprocess over a
/// freshly seeded directory, so no failure can cascade; every run must exit `0` with empty
/// stderr and byte-identical stdout.
#[test]
fn the_in_process_compile_route_answers_the_same_at_every_staging_path_length() {
    let Some(oracle) = support::require_oracle("path length independence") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` on $PATH (and no `$ERLC`), so the fixture cannot be compiled");
        return;
    };
    let (beam_bytes, _build_dir) = build_fixture(&erlc);
    let started = Instant::now();
    // The first length's answer is the reference every later length must reproduce byte for
    // byte; it is also the one the system oracle is asked about (below).
    let mut reference: Option<(usize, Vec<u8>)> = None;
    let mut staged = 0usize;
    for &length in &LENGTHS {
        let Some(dir) = staged_dir(length, "stage") else {
            continue;
        };
        // Per attempt, re-seed EVERY input: the runtime's own compile writes the `.beam` into
        // this directory, and a failed run may leave it truncated or gone.
        let beam = dir.0.join(BEAM_FILE);
        fs::write(&beam, &beam_bytes).expect("the staged .beam is written");
        fs::write(dir.0.join(SOURCE_FILE), SELFHOST_DOCFILE_SOURCE)
            .expect("the staged source is written");
        fs::write(dir.0.join(DOC_FILE), b"x").expect("the staged doc placeholder is written");
        let staged_path = beam.to_str().expect("the scratch path is UTF-8");
        let spec = format!("{MODULE}:compile_self/0");
        let output: Output = support::run_bounded_with_env(
            Path::new(BINARY),
            &["run", staged_path, &spec],
            &[],
        )
        .unwrap_or_else(|| {
            panic!(
                "`beam_rt run` over the module staged at absolute path length {length} did not \
                 answer within {}s",
                support::PROBE_TIMEOUT.as_secs()
            )
        });
        assert_eq!(
            output.status.code(),
            Some(0),
            "the in-process compile route at absolute path length {length} did not exit 0 \
             (staged at {staged_path}); stderr: {}",
            excerpt(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "the in-process compile route at absolute path length {length} wrote to stderr: {}",
            excerpt(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            EXPECTED_STDOUT,
            "the in-process compile route at absolute path length {length} answered {:?}, not {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(EXPECTED_STDOUT)
        );
        match &reference {
            None => {
                reference = Some((length, output.stdout));
                // The golden is the **oracle's** answer, cross-checked once on this very dir.
                let expr = format!(
                    "V = {MODULE}:compile_self(), io:format(\"~p~n\", [V]), halt(0)."
                );
                if let Some(answer) = support::oracle_eval(&oracle, Some(&dir.0), &expr) {
                    assert_eq!(
                        answer,
                        String::from_utf8_lossy(EXPECTED_STDOUT).trim(),
                        "the system `erl`'s answer for the same staged module"
                    );
                } else {
                    eprintln!(
                        "note: the system oracle could not answer the same staged module; the \
                         golden was checked against the fixed runtime only"
                    );
                }
            }
            Some((first, expected)) => assert_eq!(
                output.stdout, *expected,
                "the in-process compile route answered differently at absolute path length \
                 {length} than at length {first}"
            ),
        }
        staged += 1;
    }
    if staged < MIN_STAGED_LENGTHS {
        skip_loudly(
            "this host's temp directory leaves room for fewer than the required probe lengths",
        );
        return;
    }
    debug_assert!(
        staged >= MIN_STAGED_LENGTHS,
        "at least {MIN_STAGED_LENGTHS} of the {} requested absolute path lengths must be \
         stageable on this host (staged {staged})",
        LENGTHS.len()
    );
    let elapsed = started.elapsed();
    println!(
        "path length independence: {staged}/{} lengths agree (exit 0, empty stderr, stdout {:?}) \
         in {:.1}s",
        LENGTHS.len(),
        String::from_utf8_lossy(EXPECTED_STDOUT),
        elapsed.as_secs_f64()
    );
}
