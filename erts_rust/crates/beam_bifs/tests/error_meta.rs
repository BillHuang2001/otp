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
//! `erlang:error/2,3`'s stacktrace-side payload: `beam_bifs::error_meta`'s **option rule**
//! against the C's `error_3` (`../../erts/emulator/beam/bif.c:1462-1503`), and the
//! raise class/reason those two BIFs must keep.
//!
//! Two independent sources per case, never a value asserted against itself:
//!
//! * this crate's [`error_meta`] answer for the same frozen-layout option list, and
//! * the **live system oracle**'s own rendering of the innermost caught stacktrace
//!   item. ERTS stores `{Reason, Args}` (or `{Reason, Args, ErrorInfo}` for `error/3`)
//!   as the exception value and `erts_build_mfa_item`
//!   (`../../erts/emulator/beam/utils.c:3430-3457`) renders the argument list into the
//!   item's `A` slot and conses `{file, …}`/`{line, …}` **onto** the `error_info` term,
//!   so a valid `error_info` shows up as the last element of that item's `Location`
//!   list. The probe reports it as the sorted key list of the map
//!   (`{some,[a]}`), which distinguishes "last wins" from "first wins" without pinning
//!   a map's print order.
//!
//! The matrix (measured on the oracle, and asserted here on both sides):
//!
//! ```text
//! []                                        ⇒ none
//! [{error_info, #{}}]                       ⇒ {some,[]}
//! [{error_info, #{a=>1}}]                   ⇒ {some,[a]}
//! [{error_info, #{a=>1}},{error_info,#{b=>2}}] ⇒ {some,[b]}      (last wins)
//! [{error_info, bogus}]                     ⇒ none  (a non-map candidate)
//! [{bogus, 1}]                              ⇒ none  (wrong tag)
//! [bogus]                                   ⇒ none  (not a tuple)
//! [{error_info, #{a=>1}} | tail]            ⇒ none  (improper: invalidated)
//! notalist                                  ⇒ none  (not a list)
//! [{error_info, #{a=>1}, x}]                ⇒ none  (arity ≠ 2)
//! [{error_info, #{a=>1}}, bogus]            ⇒ none  (a later illegal element)
//! [[{error_info, #{a=>1}}]]                 ⇒ none  (a list element is not a tuple)
//! ```
//!
//! The `error/2` shape contributes no `error_info` at all, and a non-list `Args` is
//! stored verbatim by this crate — the C's degradation there is the **renderer**'s
//! (`is_list(args) || is_nil(args)`, else the plain arity), which the probe pins as
//! `args=0` for a zero-arity fun.
//!
//! The oracle test is **fail-loud in the crate's live-oracle style**: with no usable
//! `erl`/`erlc` it prints a loud banner and returns — which is *not* a pass — and once
//! the toolchain runs, any oracle/compile failure panics with the transcript attached.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, error_meta, BifContext, ErrorMeta};
use beam_codec::term::{make_flatmap, map_pairs_storage_order};
use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, AtomTable, Eterm, NIL};

/// The atoms every case resolves: the option tag, the two map keys, the illegal shapes
/// and the raise reasons.
const SEED: [&str; 9] = [
    "error_info",
    "a",
    "b",
    "bogus",
    "x",
    "tail",
    "notalist",
    "foo",
    "hello",
];

fn seeded() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(SEED);
    atoms
}

/// One row of the option matrix.
struct Case {
    /// The fixture's case name — also the oracle transcript's key.
    name: &'static str,
    /// The `error/3` option list, or `None` for the `error/2` shape.
    options: Option<Eterm>,
    /// The map `error_meta` must keep, by **term identity** with the map put into the
    /// option list (`None` = no `error_info` survives).
    expect: Option<Eterm>,
}

/// Builds the whole matrix on `ctx`'s heap, in the fixture's order.
fn cases(ctx: &mut BifContext<'_>) -> Vec<Case> {
    let error_info = ctx.atom("error_info").expect("seeded");
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let bogus = ctx.atom("bogus").expect("seeded");
    let x = ctx.atom("x").expect("seeded");
    let tail = ctx.atom("tail").expect("seeded");
    let notalist = ctx.atom("notalist").expect("seeded");
    let one = make_small(1);

    let empty = make_flatmap(ctx.heap(), &[]).expect("the empty map fits");
    let map_a = make_flatmap(ctx.heap(), &[(a, one)]).expect("the one-pair map fits");
    let map_b = make_flatmap(ctx.heap(), &[(b, one)]).expect("the one-pair map fits");

    let info = |ctx: &mut BifContext<'_>, value: Eterm| {
        ctx.tuple(&[error_info, value]).expect("the 2-tuple fits")
    };
    let list =
        |ctx: &mut BifContext<'_>, elems: &[Eterm]| ctx.list(elems).expect("the option list fits");

    // The conforming shapes.
    let empty_info = info(ctx, empty);
    let a_info = info(ctx, map_a);
    let b_info = info(ctx, map_b);
    let empty_list = list(ctx, &[]);
    let only_empty = list(ctx, &[empty_info]);
    let only_a = list(ctx, &[a_info]);
    let both = list(ctx, &[a_info, b_info]);

    // The illegal shapes.
    let not_a_map = info(ctx, bogus);
    let wrong_tag = ctx.tuple(&[bogus, one]).expect("fits");
    let three_tuple = ctx.tuple(&[error_info, map_a, x]).expect("fits");

    vec![
        Case {
            name: "empty_list",
            options: Some(empty_list),
            expect: None,
        },
        Case {
            name: "empty_map",
            options: Some(only_empty),
            expect: Some(empty),
        },
        Case {
            name: "one_info",
            options: Some(only_a),
            expect: Some(map_a),
        },
        Case {
            name: "two_infos",
            options: Some(both),
            expect: Some(map_b),
        },
        Case {
            name: "info_not_map",
            options: Some(list(ctx, &[not_a_map])),
            expect: None,
        },
        Case {
            name: "bogus_tuple",
            options: Some(list(ctx, &[wrong_tag])),
            expect: None,
        },
        Case {
            name: "bogus_elem",
            options: Some(list(ctx, &[bogus])),
            expect: None,
        },
        Case {
            name: "improper",
            options: Some(ctx.cons(a_info, tail).expect("the improper list fits")),
            expect: None,
        },
        Case {
            name: "not_a_list",
            options: Some(notalist),
            expect: None,
        },
        Case {
            name: "arity2",
            options: None,
            expect: None,
        },
        Case {
            name: "three_tuple",
            options: Some(list(ctx, &[three_tuple])),
            expect: None,
        },
        Case {
            name: "info_then_bogus",
            options: Some(list(ctx, &[a_info, bogus])),
            expect: None,
        },
        Case {
            name: "nested_list_elem",
            options: Some(list(ctx, &[only_a])),
            expect: None,
        },
    ]
}

/// The argument list every fixture case raises with (`[1,2]`), and the crate's answer
/// for it: `args` is stored **verbatim**, so the same term comes back out.
#[test]
fn error_meta_keeps_the_option_matrix() {
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

    for case in cases(&mut ctx) {
        assert_eq!(
            error_meta(args, case.options, &atoms),
            ErrorMeta {
                args,
                error_info: case.expect
            },
            "case `{}`",
            case.name
        );
    }
}

/// A non-list `Args` is kept verbatim too — never validated, never replaced by the
/// declared arity (`error(foo, notalist)` is `error:foo` on the oracle, and the
/// *renderer* is what falls back to the plain arity).
#[test]
fn error_meta_stores_a_non_list_argument_verbatim() {
    let mut heap = Heap::new(64).expect("test heap");
    let atoms = seeded();
    let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let notalist = ctx.atom("notalist").expect("seeded");

    assert_eq!(
        error_meta(notalist, None, &atoms),
        ErrorMeta {
            args: notalist,
            error_info: None
        }
    );
}

/// The **reason class** of the raise BIFs stays exactly as it is today — that half of
/// the contract is already oracle-exact, and this fix does not touch it: `error:foo` for
/// an atom reason, the live term for a compound one, and no `badarg` for any of the
/// illegal option shapes.
#[test]
fn error_2_and_error_3_keep_their_reason_and_class() {
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let registry = default_registry();

    let foo = ctx.atom("foo").expect("seeded");
    let hello = ctx.atom("hello").expect("seeded");
    let notalist = ctx.atom("notalist").expect("seeded");
    let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
    let bad = ctx
        .tuple(&[ctx.atom("bogus").expect("seeded"), make_small(1)])
        .expect("fits");

    /// Runs a raise BIF and returns its `(class, reason, live reason term)`.
    fn raised(result: beam_common::Result<Eterm>) -> (ExceptionClass, ExitReason, Option<Eterm>) {
        match result {
            Err(BeamError::Raise(raise)) => {
                let live = raise
                    .live_reason()
                    .and_then(|payload| payload.downcast_ref::<Eterm>().copied());
                (raise.class, raise.reason, live)
            }
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    // `error(foo)`, `error(foo, [1,2])` and `error(foo, [1,2], <anything>)` are all
    // `error:foo` — the option list never reaches the reason.
    for arity in [1u32, 2, 3] {
        let call_args: &[Eterm] = match arity {
            1 => &[foo],
            2 => &[foo, args],
            _ => &[foo, args, notalist],
        };
        assert_eq!(
            raised(registry.call("erlang", "error", arity, &mut ctx, call_args)),
            (ExceptionClass::Error, ExitReason::atom("foo"), None),
            "erlang:error/{arity}"
        );
    }

    // `error({bogus,1}, [1,2], [])` ⇒ `error:{bogus,1}` — a compound reason rides the
    // live slot, unchanged.
    assert_eq!(
        raised(registry.call("erlang", "error", 3, &mut ctx, &[bad, args, NIL])),
        (ExceptionClass::Error, ExitReason::Structured, Some(bad))
    );
    // `error(hello, [], [{bogus,1}])` ⇒ `error:hello` (an illegal option is not a raise).
    let opts = ctx.list(&[bad]).expect("fits");
    assert_eq!(
        raised(registry.call("erlang", "error", 3, &mut ctx, &[hello, NIL, opts])),
        (ExceptionClass::Error, ExitReason::atom("hello"), None)
    );
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// The fixture module, compiled into a scratch directory by `erlc`.
///
/// Every case is a zero-arity fun, so the innermost stacktrace item's `A` slot renders
/// the argument list when the C kept one and the plain arity (`0`) when it did not —
/// `erts_build_mfa_item`'s `is_list(args) || is_nil(args)` split. `error_info_of/1`
/// scans the item's `Location` list, which is where the C conses `{error_info, Map}`
/// (last), and reports its sorted keys so the transcript does not depend on a map's
/// print order.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_bifs/tests/error_meta.rs` (never committed).
-module(ermetaprobe).
-export([run/0]).

run() ->
    Cases = [
      {empty_list,      fun() -> erlang:error(boom, [1,2], []) end},
      {empty_map,       fun() -> erlang:error(boom, [1,2], [{error_info, #{}}]) end},
      {one_info,        fun() -> erlang:error(boom, [1,2], [{error_info, #{a => 1}}]) end},
      {two_infos,       fun() -> erlang:error(boom, [1,2], [{error_info, #{a => 1}},
                                                            {error_info, #{b => 2}}]) end},
      {info_not_map,    fun() -> erlang:error(boom, [1,2], [{error_info, bogus}]) end},
      {bogus_tuple,     fun() -> erlang:error(boom, [1,2], [{bogus, 1}]) end},
      {bogus_elem,      fun() -> erlang:error(boom, [1,2], [bogus]) end},
      {improper,        fun() -> erlang:error(boom, [1,2], [{error_info, #{a => 1}} | tail]) end},
      {not_a_list,      fun() -> erlang:error(boom, [1,2], notalist) end},
      {arity2,          fun() -> erlang:error(boom, [1,2]) end},
      {three_tuple,     fun() -> erlang:error(boom, [1,2], [{error_info, #{a => 1}, x}]) end},
      {info_then_bogus, fun() -> erlang:error(boom, [1,2], [{error_info, #{a => 1}}, bogus]) end},
      {nested_list_elem, fun() -> erlang:error(boom, [1,2], [[{error_info, #{a => 1}}]]) end}
    ],
    lists:foreach(fun({Name, F}) ->
        {Args, Info} = probe(F),
        io:format("~p args=~w info=~s~n", [Name, Args, Info])
    end, Cases).

probe(F) ->
    try F() catch error:boom:S ->
        [Top | _] = S,
        {element(3, Top), error_info_of(element(4, Top))}
    end.

error_info_of(L) when is_list(L) -> error_info_of_1(L);
error_info_of(_) -> "none".
error_info_of_1([{error_info, M} | _]) ->
    io_lib:format("{some,~w}", [lists:sort(maps:keys(M))]);
error_info_of_1([_ | T]) -> error_info_of_1(T);
error_info_of_1([]) -> "none".
"#;

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

/// `name` in the first `$PATH` directory that holds an executable of that name.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The system `erl`/`erlc` pair — `$ERTS_ORACLE` (then its directory) when that names an
/// executable, else `$PATH`.
fn resolve_toolchain() -> Option<(PathBuf, PathBuf)> {
    let erl = match std::env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let program = PathBuf::from(&value);
            if !is_executable(&program) {
                eprintln!(
                    "[beam_bifs error_meta] $ERTS_ORACLE='{}' is not an existing executable — \
                     treating it as no oracle.",
                    program.display()
                );
                return None;
            }
            program
        }
        None => on_path("erl")?,
    };
    let erlc = match erl.parent().map(|dir| dir.join("erlc")) {
        Some(sibling) if is_executable(&sibling) => sibling,
        _ => on_path("erlc")?,
    };
    Some((erl, erlc))
}

/// Prints the loud "no oracle, not a pass" banner.
fn skip_banner() {
    eprintln!();
    eprintln!("============ beam_bifs error_meta: live oracle SKIPPED ============");
    eprintln!("No usable `erl`/`erlc` pair found ($ERTS_ORACLE unset or not");
    eprintln!("executable, and no `erl`/`erlc` on $PATH).");
    eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
    eprintln!("error/2,3's option matrix is then ONLY crate-side asserted, never");
    eprintln!("cross-checked against the C's error_3 rule it models.");
    eprintln!("===================================================================");
    eprintln!();
}

/// A `$TMPDIR` scratch directory holding the compiled fixture.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Creates the directory, writes the fixture source and compiles it.
    fn build(erlc: &Path) -> Scratch {
        let dir = std::env::temp_dir().join(format!("beam_bifs_error_meta_{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        let source = dir.join("ermetaprobe.erl");
        std::fs::write(&source, FIXTURE_ERL)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", source.display()));
        let output = Command::new(erlc)
            .arg("-o")
            .arg(&dir)
            .arg(&source)
            .output()
            .unwrap_or_else(|e| panic!("cannot run `{}`: {e}", erlc.display()));
        assert!(
            output.status.success(),
            "`{}` exited with {:?}\n--- stderr ---\n{}",
            erlc.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            dir.join("ermetaprobe.beam").is_file(),
            "`erlc` produced no ermetaprobe.beam in {}",
            dir.display()
        );
        Scratch { dir }
    }

    /// The scratch directory (the fixture's code path).
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// Runs `erl -noshell -pa <code_path> -eval <eval>` and returns its stdout. Panics on a
/// spawn failure or a non-zero exit, so a broken oracle can never look like a pass.
fn run_erl(erl: &Path, code_path: &Path, eval: &str) -> String {
    let output = Command::new(erl)
        .arg("-noshell")
        .arg("-pa")
        .arg(code_path)
        .arg("-eval")
        .arg(eval)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", erl.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        erl.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// The `(args, info)` rendering the oracle produced for case `name`.
fn oracle_case(transcript: &str, name: &str) -> (String, String) {
    let prefix = format!("{name} ");
    let line = transcript
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the oracle transcript has no `{name}` line:\n{transcript}"));
    let (args, info) = line
        .split_once(" info=")
        .unwrap_or_else(|| panic!("`{name}` line has no `info=`: {line}"));
    let args = args
        .strip_prefix("args=")
        .unwrap_or_else(|| panic!("`{name}` line has no `args=`: {line}"));
    (args.to_string(), info.to_string())
}

/// The oracle's `info=` rendering of a kept map, derived from the **same term** the
/// crate must hand back: `{some,[<sorted key names>]}`.
fn info_rendering(atoms: &AtomTable, map: Eterm) -> String {
    let pairs = map_pairs_storage_order(map).expect("the kept error_info is a map");
    let mut keys: Vec<String> = pairs
        .iter()
        .map(|(key, _)| {
            atoms
                .name(atom_val(*key))
                .unwrap_or_else(|| panic!("an error_info key is not in the table"))
                .to_string()
        })
        .collect();
    keys.sort();
    format!("{{some,[{}]}}", keys.join(","))
}

/// The whole matrix, cross-checked against the live oracle: for each case the oracle's
/// rendering of the caught innermost item must be exactly what [`error_meta`] implies
/// (`none`, or `{some,[keys]}` of the map it kept).
#[test]
fn the_live_oracle_agrees_on_the_whole_option_matrix() {
    let Some((erl, erlc)) = resolve_toolchain() else {
        skip_banner();
        return;
    };
    let scratch = Scratch::build(&erlc);
    let transcript = run_erl(&erl, scratch.path(), "ermetaprobe:run(),halt().");

    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

    for case in cases(&mut ctx) {
        let (oracle_args, oracle_info) = oracle_case(&transcript, case.name);
        // The argument list reaches the innermost item's `A` slot for every case — that
        // is the payload this crate cannot attach yet, not something the option rule
        // decides.
        assert_eq!(
            oracle_args, "[1,2]",
            "case `{}`: the oracle's innermost item carries the argument list",
            case.name
        );
        let expected = match case.expect {
            None => "none".to_string(),
            Some(map) => info_rendering(&atoms, map),
        };
        assert_eq!(
            oracle_info, expected,
            "case `{}`: the oracle's error_info disagrees with the kept map",
            case.name
        );
        // …and the crate's answer for the same list implies that same rendering.
        let meta = error_meta(args, case.options, &atoms);
        assert_eq!(
            meta.error_info, case.expect,
            "case `{}`: error_meta kept a different map",
            case.name
        );
        assert_eq!(
            meta.args, args,
            "case `{}`: error_meta lost the args",
            case.name
        );
    }
}
