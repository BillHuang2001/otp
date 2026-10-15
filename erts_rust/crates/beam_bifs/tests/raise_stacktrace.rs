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

//! `erlang:raise/3`'s **stacktrace forwarding**, cross-checked field by field
//! against the live system oracle.
//!
//! `raise(Class, Reason, Stacktrace)` decodes its 3rd argument into the returned
//! raise's [`beam_common::StackTrace`] (`src/convert.rs::decode_stacktrace`), which
//! the engine renders **verbatim** in place of its own cooked trace — the oracle's
//! `build_stacktrace` returns a caller-supplied trace unchanged, so the caught `S`
//! is the supplied list, normalized only in that a 3-element `{M,F,A}` frame comes
//! back as `{M,F,A,[]}`.
//!
//! Every probe below is evaluated **twice**: once by the system oracle, and once
//! through this crate's public [`BifRegistry::call`]. Both sides print the same
//! lines — the cooked trace in the oracle's own `~p` spelling, then one field line
//! per frame (`Module:Function/Arity@File:Line`, `@-` for an absent location) — and
//! the two transcripts are compared line by line. The oracle's `~p` output is the
//! ground truth for the trace, so the comparison is field by field: module,
//! function, arity, source file, line, and location presence.
//!
//! The oracle is resolved the same fail-loud way the sibling integration files do
//! (`$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`): with
//! no usable oracle the test prints a loud "this is NOT a pass" notice and skips, and
//! a present oracle that exits non-zero or prints an unexpected number of lines
//! panics. It is never `#[ignore]`d.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{AtomName, BeamError, ExceptionClass, ExitReason, Raise, SourceLocation, StackFrame};
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

/// The probes, in the order both sides print them: the Erlang `erlang:raise/3`
/// expression (ridden down to the caught cooked trace `S`) and the same trace the
/// Rust side builds (`probe_arguments`).
///
/// Probe 0 is the **fully specified** literal the oracle echoes verbatim; probe 1
/// pairs a located frame with a bare one; probes 2/3 are the absent-location arms;
/// probe 4 is the empty trace (which decodes to the empty `StackTrace`, so the
/// engine's supplied-trace latch stays unarmed); probe 5 pairs a **non-atom** reason
/// with a located frame; probe 6 exercises a second located frame.
const PROBES: &[(&str, &str)] = &[
    (
        "one fully specified frame",
        "try erlang:raise(error, foo, \
         [{lists,map,2,[{file,\"lists.erl\"},{line,1234}]}]) \
         catch _:_:S -> S end",
    ),
    (
        "a located frame and a bare frame",
        "try erlang:raise(error, foo, \
         [{lists,map,2,[{file,\"lists.erl\"},{line,1234}]},{m,f,1}]) \
         catch _:_:S -> S end",
    ),
    (
        "an absent location",
        "try erlang:raise(error, foo, [{m,f,1}]) catch _:_:S -> S end",
    ),
    (
        "an explicit empty location",
        "try erlang:raise(error, foo, [{m,f,1,[]}]) catch _:_:S -> S end",
    ),
    (
        "the empty trace",
        "try erlang:raise(error, foo, []) catch _:_:S -> S end",
    ),
    (
        "a non-atom reason with a located frame",
        "try erlang:raise(throw, {a,b}, [{m,f,1,[{file,\"m.erl\"},{line,7}]}]) \
         catch _:_:S -> S end",
    ),
    (
        "two frames, the second located",
        "try erlang:raise(exit, foo, [{m,f,1},{n,g,2,[{file,\"x.erl\"},{line,42}]}]) \
         catch _:_:S -> S end",
    ),
];

// ---------------------------------------------------------------------------
// Building the probes on this crate's side
// ---------------------------------------------------------------------------

/// The names every probe's terms are built from (the frame names, the location
/// keys, and the class/reason atoms).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "ok", "badarg", "error", "exit", "throw", "foo", "a", "b", "m", "f",
        "n", "g", "lists", "map", "file", "line",
    ]);
    atoms
}

/// A char list of the code points of `s` — the `{file, "name"}` payload.
fn char_list(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
    let mut elems = Vec::with_capacity(s.chars().count());
    for ch in s.chars() {
        elems.push(ctx.int(i64::from(u32::from(ch))).expect("a code point fits"));
    }
    ctx.list(&elems).expect("the char list fits")
}

/// The `{Module, Function, Arity}` / `{Module, Function, Arity, Location}` frame
/// term, with the location the exact `[{file, File}, {line, Line}]` shape.
fn frame(
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: i64,
    location: Option<(&str, i64)>,
) -> Eterm {
    let module = ctx.atom(module).expect("seeded");
    let function = ctx.atom(function).expect("seeded");
    let arity = ctx.int(arity).expect("an arity fits");
    let Some((file, line)) = location else {
        return ctx
            .tuple(&[module, function, arity])
            .expect("the bare frame fits");
    };
    let name = char_list(ctx, file);
    let line = ctx.int(line).expect("a line fits");
    let file_key = ctx.atom("file").expect("seeded");
    let line_key = ctx.atom("line").expect("seeded");
    let file_pair = ctx.tuple(&[file_key, name]).expect("the file pair fits");
    let line_pair = ctx.tuple(&[line_key, line]).expect("the line pair fits");
    let location = ctx
        .list(&[file_pair, line_pair])
        .expect("the location fits");
    ctx.tuple(&[module, function, arity, location])
        .expect("the located frame fits")
}

/// The `(Class, Reason, Stacktrace)` arguments of probe `index`.
fn probe_arguments(index: usize, ctx: &mut BifContext<'_>) -> [Eterm; 3] {
    let class = match index {
        5 => ctx.atom("throw").expect("seeded"),
        _ => ctx.atom("error").expect("seeded"),
    };
    let reason = match index {
        // `{a, b}` — a **non-atom** reason, which rides the live-reason slot.
        5 => {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            ctx.tuple(&[a, b]).expect("the reason fits")
        }
        _ => ctx.atom("foo").expect("seeded"),
    };
    let trace = match index {
        0 => {
            let only = frame(ctx, "lists", "map", 2, Some(("lists.erl", 1234)));
            ctx.list(&[only]).expect("the trace fits")
        }
        1 => {
            let first = frame(ctx, "lists", "map", 2, Some(("lists.erl", 1234)));
            let second = frame(ctx, "m", "f", 1, None);
            ctx.list(&[first, second]).expect("the trace fits")
        }
        2 => {
            let only = frame(ctx, "m", "f", 1, None);
            ctx.list(&[only]).expect("the trace fits")
        }
        3 => {
            // The explicit empty location: `{m, f, 1, []}`.
            let module = ctx.atom("m").expect("seeded");
            let function = ctx.atom("f").expect("seeded");
            let arity = ctx.int(1).expect("an arity fits");
            let empty = ctx.list(&[]).expect("the empty location fits");
            let only = ctx
                .tuple(&[module, function, arity, empty])
                .expect("the frame fits");
            ctx.list(&[only]).expect("the trace fits")
        }
        4 => ctx.list(&[]).expect("the empty trace fits"),
        5 => {
            let only = frame(ctx, "m", "f", 1, Some(("m.erl", 7)));
            ctx.list(&[only]).expect("the trace fits")
        }
        _ => {
            let first = frame(ctx, "m", "f", 1, None);
            let second = frame(ctx, "n", "g", 2, Some(("x.erl", 42)));
            ctx.list(&[first, second]).expect("the trace fits")
        }
    };
    [class, reason, trace]
}

/// Runs probe `index` through `erlang:raise/3` and returns the raise it produced.
fn run_probe(reg: &BifRegistry, index: usize) -> Raise {
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let args = probe_arguments(index, &mut ctx);
    match reg.call("erlang", "raise", 3, &mut ctx, &args) {
        Err(BeamError::Raise(raise)) => raise,
        other => panic!("probe {index}: expected a raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The shared rendering
// ---------------------------------------------------------------------------

/// One frame in the oracle's `~p` spelling: `{Module,Function,Arity,Location}`,
/// with the location the `[{file,"Name"},{line,N}]` pair or the empty list `[]` the
/// oracle normalizes an absent location to.
fn render_frame(frame: &StackFrame) -> String {
    let location = match &frame.location {
        Some(location) => format!(
            "[{{file,\"{}\"}},{{line,{}}}]",
            location.file.as_str(),
            location.line
        ),
        None => "[]".to_owned(),
    };
    format!(
        "{{{},{},{},{}}}",
        frame.module.as_str(),
        frame.function.as_str(),
        frame.arity,
        location
    )
}

/// The whole trace in the oracle's `~p` spelling.
fn render_trace(frames: &[StackFrame]) -> String {
    let items: Vec<String> = frames.iter().map(render_frame).collect();
    format!("[{}]", items.join(","))
}

/// The field line for one frame: `Module:Function/Arity@File:Line`, or `@-` when
/// the frame carries no location. This is the line both sides print per frame, so
/// the comparison is over module, function, arity, file, line and presence.
fn field_line(frame: &StackFrame) -> String {
    let position = match &frame.location {
        Some(location) => format!("{}:{}", location.file.as_str(), location.line),
        None => "-".to_owned(),
    };
    format!(
        "{}:{}/{}@{}",
        frame.module.as_str(),
        frame.function.as_str(),
        frame.arity,
        position
    )
}

/// The whole transcript this crate's side prints: `raw` (the cooked trace in the
/// oracle's `~p` spelling), one `frame` line per frame, then `end`.
fn transcript(raises: &[Raise]) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, raise) in raises.iter().enumerate() {
        let frames = &raise.stacktrace.frames;
        lines.push(format!("{index} raw {}", render_trace(frames)));
        for frame in frames {
            lines.push(format!("{index} frame {}", field_line(frame)));
        }
        lines.push(format!("{index} end"));
    }
    lines
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` and the sibling test files use.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs raise_stacktrace] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The oracle's transcript: for each probe, the caught cooked trace `S` as `~p`
/// (the raw line), then one field line per frame of `S`, then `end` — exactly the
/// lines [`transcript`] prints for this crate.
fn oracle_lines(oracle: &Path, probes: &[(&str, &str)]) -> Vec<String> {
    let mut eval = String::from(
        "P = fun(I, E) -> \
           S = E(), \
           io:format(\"~w raw ~s~n\", [I, lists:flatten(io_lib:format(\"~p\", [S]))]), \
           lists:foreach(fun(F) -> \
             {M, Fn, A, Loc} = case F of \
               {M0, F0, A0} -> {M0, F0, A0, []}; \
               {M0, F0, A0, L0} -> {M0, F0, A0, L0} \
             end, \
             Pos = case Loc of \
               [] -> \"-\"; \
               [{file, Name}, {line, Ln}] -> \
                 lists:flatten(io_lib:format(\"~s:~w\", [Name, Ln])) \
             end, \
             io:format(\"~w frame ~s:~s/~w@~s~n\", [I, M, Fn, A, Pos]) \
           end, S), \
           io:format(\"~w end~n\", [I]) \
         end, ",
    );
    for (index, (_, expression)) in probes.iter().enumerate() {
        eval.push_str(&format!("P({index}, fun() -> {expression} end), "));
    }
    eval.push_str("ok");

    let output = Command::new(oracle)
        .args(["-noshell", "-eval", &eval, "-s", "init", "stop"])
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The oracle's cooked trace and this crate's decoded frames agree field by field,
/// for every probe. Skips loudly when no oracle is available (it is not a pass
/// then).
#[test]
fn raise_stacktrace_matches_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("======= beam_bifs raise_stacktrace: live oracle SKIPPED =======");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass — set");
        eprintln!("$ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };

    let registry = default_registry();
    let raises: Vec<Raise> = (0..PROBES.len()).map(|index| run_probe(&registry, index)).collect();
    let ours = transcript(&raises);
    let theirs = oracle_lines(&oracle, PROBES);    assert_eq!(
        theirs.len(),
        ours.len(),
        "the oracle printed {} lines, this crate {}: they are not the same transcript\n\
         --- oracle ---\n{}\n--- rust ---\n{}",
        theirs.len(),
        ours.len(),
        theirs.join("\n"),
        ours.join("\n"),
    );
    for (index, (theirs, ours)) in theirs.iter().zip(ours.iter()).enumerate() {
        assert_eq!(theirs, ours, "transcript line {index}");
    }

    // Probe 0 pins the semantics: the oracle echoes the fully specified literal
    // verbatim and its cooked `S` is exactly that literal.
    assert_eq!(
        theirs[0],
        "0 raw [{lists,map,2,[{file,\"lists.erl\"},{line,1234}]}]"
    );
    assert_eq!(theirs[1], "0 frame lists:map/2@lists.erl:1234");
    assert_eq!(theirs[2], "0 end");
    assert_eq!(
        raises[0].stacktrace.frames,
        vec![StackFrame {
            module: AtomName::new("lists"),
            function: AtomName::new("map"),
            arity: 2,
            location: Some(SourceLocation {
                file: AtomName::new("lists.erl"),
                line: 1234,
            }),
        }]
    );

    // Probe 1: the located frame and the bare frame (whose absent location decodes
    // to `None`), in the trace's own order.
    assert_eq!(
        raises[1].stacktrace.frames,
        vec![
            StackFrame {
                module: AtomName::new("lists"),
                function: AtomName::new("map"),
                arity: 2,
                location: Some(SourceLocation {
                    file: AtomName::new("lists.erl"),
                    line: 1234,
                }),
            },
            StackFrame::new(AtomName::new("m"), AtomName::new("f"), 1),
        ]
    );
    // Probes 2 and 3 — `{m,f,1}` and `{m,f,1,[]}` — decode identically.
    assert_eq!(raises[2].stacktrace, raises[3].stacktrace);
    // Probe 4 decodes to the EMPTY trace, so the engine's supplied-trace latch is
    // never armed and the raise keeps its derived stacktrace.
    assert!(raises[4].stacktrace.is_empty());
    // The reason is orthogonal to the trace: probe 5's non-atom reason rides the
    // live-reason slot while its frames come through.
    assert_eq!(raises[5].class, ExceptionClass::Throw);
    assert_eq!(raises[5].reason, ExitReason::Structured);
    assert!(raises[5].live_reason().is_some());
    assert_eq!(
        raises[5].stacktrace.frames,
        vec![StackFrame {
            module: AtomName::new("m"),
            function: AtomName::new("f"),
            arity: 1,
            location: Some(SourceLocation {
                file: AtomName::new("m.erl"),
                line: 7,
            }),
        }]
    );
    // ... and the atom-reason probes carry no live slot.
    for raise in [&raises[0], &raises[1], &raises[2], &raises[3], &raises[4], &raises[6]] {
        assert_eq!(raise.class, ExceptionClass::Error);
        assert_eq!(raise.reason, ExitReason::atom("foo"));
        assert!(raise.live_reason().is_none());
    }
}
