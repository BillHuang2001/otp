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
use beam_bifs::{
    default_registry, error_cause, error_meta, BifContext, ErrorCause, ErrorMeta, TableRegistry,
};
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
    /// The `error/3` option-matrix fixture.
    fn build(erlc: &Path) -> Scratch {
        Scratch::build_named(erlc, "ermetaprobe", FIXTURE_ERL)
    }

    /// Creates a directory, writes `source` as `<module>.erl` and compiles it.
    fn build_named(erlc: &Path, module: &str, source: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "beam_bifs_error_meta_{module}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        let source_path = dir.join(format!("{module}.erl"));
        std::fs::write(&source_path, source)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", source_path.display()));
        let output = Command::new(erlc)
            .arg("-o")
            .arg(&dir)
            .arg(&source_path)
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
            dir.join(format!("{module}.beam")).is_file(),
            "`erlc` produced no {module}.beam in {}",
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

// ---------------------------------------------------------------------------
// The `error_info` map's `cause` half
// ---------------------------------------------------------------------------
//
// A failing BIF's innermost stacktrace item carries
// `{error_info, #{module => FormatModule}}`, and for the failures the format module
// can describe it carries `#{cause => Value, module => FormatModule}` — the C's
// `EXF_HAS_EXT_INFO` flag and its `MAP2(am_cause, c_p->fvalue, am_module, …)` packing
// (`../../erts/emulator/beam/error.h:78`, `beam_common.c:994-1004`). `fvalue` is the
// `EXI_*` value of the failing `ets`/`register` operation, translated by
// `erl_erts_errors:format_error/1` / `erl_stdlib_errors`' own tables.
//
// [`beam_bifs::error_cause`] models that value. The matrix below is measured on the
// live oracle row by row: for each case the probe reports what the innermost item
// carries (`no_cause` = a BIF frame whose `error_info` has no `cause` key, `no_info` =
// no BIF frame at all), and this file asserts both that the oracle says what the table
// claims **and** that the crate's answer for the same `(module, function, args)` is
// either exactly that value or an honest `Undecided`.

/// The argument list a cause row feeds to the crate, as the shape that decides it.
#[derive(Clone, Copy)]
enum Args {
    /// A name atom no live table binds (`unknown_atom_xyz`).
    UnknownName,
    /// The integer `1` — not a table identifier at all.
    Integer,
    /// This process's own pid — not a table identifier either.
    Pid,
    /// A tid for a table that has since been deleted (the M1 handle ambiguity).
    DeadTid,
    /// The live table's own term — resolution succeeds, so the failure is elsewhere.
    LiveTable,
    /// `erlang:register/2` with a non-atom name.
    RegisterNonAtomName,
    /// `erlang:register/2` with the atom `undefined` as the name.
    RegisterUndefinedName,
    /// `erlang:register/2` that passes all three of the C's pre-registry guards.
    RegisterWellFormed,
}

/// One row of the cause matrix.
#[derive(Clone, Copy)]
struct CauseCase {
    /// The fixture's case name — also the oracle transcript's key.
    name: &'static str,
    module: &'static str,
    function: &'static str,
    arity: u32,
    args: Args,
    /// What the oracle's innermost item carries: `None` = no `cause` (either no
    /// `error_info` at all or one without a `cause` key — the probe tells them apart
    /// in its rendering, and the row records the exact string).
    oracle: &'static str,
    /// What this crate must answer: `Some` = a decided `Cause` with that atom name,
    /// `None` = [`ErrorCause::Undecided`].
    expect: Option<&'static str>,
}

/// The matrix, in the fixture's order. Every `Some` row is a value this crate
/// computes; every `None` row is a class it declines (with the reason at the row).
const CAUSE_CASES: &[CauseCase] = &[
    CauseCase { name: "lookup_unknown_atom", module: "ets", function: "lookup", arity: 2, args: Args::UnknownName, oracle: "id", expect: Some("id") },
    CauseCase { name: "lookup_integer", module: "ets", function: "lookup", arity: 2, args: Args::Integer, oracle: "type", expect: Some("type") },
    CauseCase { name: "lookup_pid", module: "ets", function: "lookup", arity: 2, args: Args::Pid, oracle: "type", expect: Some("type") },
    // M1: a dead tid and a bare reference are word-indistinguishable, so `id` and "no
    // idea" cannot be told apart (`src/error_cause.rs` module docs).
    CauseCase { name: "lookup_dead_tid", module: "ets", function: "lookup", arity: 2, args: Args::DeadTid, oracle: "id", expect: None },
    CauseCase { name: "insert_unknown_atom", module: "ets", function: "insert", arity: 2, args: Args::UnknownName, oracle: "id", expect: Some("id") },
    CauseCase { name: "delete_unknown_atom", module: "ets", function: "delete", arity: 1, args: Args::UnknownName, oracle: "id", expect: Some("id") },
    CauseCase { name: "delete_integer", module: "ets", function: "delete", arity: 1, args: Args::Integer, oracle: "type", expect: Some("type") },
    CauseCase { name: "tab2list_unknown_atom", module: "ets", function: "tab2list", arity: 1, args: Args::UnknownName, oracle: "id", expect: Some("id") },
    CauseCase { name: "info_integer", module: "ets", function: "info", arity: 1, args: Args::Integer, oracle: "type", expect: Some("type") },
    CauseCase { name: "first_integer", module: "ets", function: "first", arity: 1, args: Args::Integer, oracle: "type", expect: Some("type") },
    CauseCase { name: "update_counter_unknown_atom", module: "ets", function: "update_counter", arity: 3, args: Args::UnknownName, oracle: "id", expect: Some("id") },
    CauseCase { name: "select_integer", module: "ets", function: "select", arity: 2, args: Args::Integer, oracle: "type", expect: Some("type") },
    CauseCase { name: "next_lookup_integer", module: "ets", function: "next_lookup", arity: 2, args: Args::Integer, oracle: "type", expect: Some("type") },
    // The table resolves; the rejected *object* is what carries the (no-)cause, and
    // no `EXI_*` value describes a malformed insert payload.
    CauseCase { name: "insert_live_notalist", module: "ets", function: "insert", arity: 2, args: Args::LiveTable, oracle: "no_cause", expect: None },
    // `ets:whereis/1`'s argument is a name, not a table: the C attributes nothing.
    CauseCase { name: "whereis_integer", module: "ets", function: "whereis", arity: 1, args: Args::Integer, oracle: "no_cause", expect: None },
    // `already_exists` needs `ets_new_2`'s private option parser (`erl_db.c:2836`).
    CauseCase { name: "new_duplicate_named", module: "ets", function: "new", arity: 2, args: Args::UnknownName, oracle: "already_exists", expect: None },
    // The three pre-registry refusals (`register.c:181-190`): `c_p->fvalue = am_none`.
    CauseCase { name: "register_nonpid", module: "erlang", function: "register", arity: 2, args: Args::RegisterNonAtomName, oracle: "none", expect: Some("none") },
    CauseCase { name: "register_badname", module: "erlang", function: "register", arity: 2, args: Args::RegisterNonAtomName, oracle: "none", expect: Some("none") },
    CauseCase { name: "register_undefined", module: "erlang", function: "register", arity: 2, args: Args::RegisterUndefinedName, oracle: "none", expect: Some("none") },
    // A well-formed call that still failed is the registry's answer, which only the
    // runtime knows (`am_registered_name`, `register.c:299`).
    CauseCase { name: "register_state", module: "erlang", function: "register", arity: 2, args: Args::RegisterWellFormed, oracle: "registered_name", expect: None },
    // No `cause` anywhere else: the bit-syntax BIFs, the conversions and `maps:*`
    // publish only `#{module => …}`.
    CauseCase { name: "binary_at_bitstring", module: "binary", function: "at", arity: 2, args: Args::Integer, oracle: "no_cause", expect: None },
    CauseCase { name: "list_to_float_badlist", module: "erlang", function: "list_to_float", arity: 1, args: Args::Integer, oracle: "no_cause", expect: None },
    CauseCase { name: "maps_get_notamap", module: "erlang", function: "map_get", arity: 2, args: Args::Integer, oracle: "no_cause", expect: None },
];

/// The fixture: every case as a zero-arity fun, rendered as
/// `<name> cause=<value|no_cause|no_info|no_exception(...)>`.
///
/// `cause_of/1` walks the innermost caught item's `Location` list to the `error_info`
/// map and reports its `cause` entry, so the transcript never depends on a map's print
/// order. `Named`/`Tmp` are created once: `Tmp` is deleted so its tid is dead.
const FIXTURE_CAUSE_ERL: &str = r#"%% Generated by `beam_bifs/tests/error_meta.rs` (never committed).
-module(emcauseprobe).
-export([run/0]).

run() ->
    Named = ets:new(my_tab, [named_table, public]),
    true = is_atom(Named),
    Tmp = ets:new(tmp_tab, []),
    true = ets:delete(Tmp),
    Cases = [
      {lookup_unknown_atom,         fun() -> ets:lookup(unknown_atom_xyz, k) end},
      {lookup_integer,              fun() -> ets:lookup(1, k) end},
      {lookup_pid,                  fun() -> ets:lookup(self(), k) end},
      {lookup_dead_tid,             fun() -> ets:lookup(Tmp, k) end},
      {insert_unknown_atom,         fun() -> ets:insert(unknown_atom_xyz, {k, v}) end},
      {delete_unknown_atom,         fun() -> ets:delete(unknown_atom_xyz) end},
      {delete_integer,              fun() -> ets:delete(1) end},
      {tab2list_unknown_atom,       fun() -> ets:tab2list(unknown_atom_xyz) end},
      {info_integer,                fun() -> ets:info(1) end},
      {first_integer,               fun() -> ets:first(1) end},
      {update_counter_unknown_atom, fun() -> ets:update_counter(unknown_atom_xyz, k, 1) end},
      {select_integer,              fun() -> ets:select(1, '_') end},
      {next_lookup_integer,         fun() -> ets:next_lookup(1, k) end},
      {insert_live_notalist,        fun() -> ets:insert(my_tab, notalist) end},
      {whereis_integer,             fun() -> ets:whereis(1) end},
      {new_duplicate_named,         fun() -> ets:new(my_tab, [named_table]) end},
      {register_nonpid,             fun() -> erlang:register(a, 1) end},
      {register_badname,            fun() -> erlang:register(1, self()) end},
      {register_undefined,          fun() -> erlang:register(undefined, self()) end},
      {register_state,              fun() -> erlang:register(myname, self()),
                                            erlang:register(othername, self()) end},
      {binary_at_bitstring,         fun() -> binary:at(<<1:1>>, 0) end},
      {list_to_float_badlist,       fun() -> erlang:list_to_float([1]) end},
      {maps_get_notamap,            fun() -> maps:get(k, notamap) end}
    ],
    lists:foreach(fun({Name, F}) ->
        Out = try F() of
                  V -> io_lib:format("no_exception(~p)", [V])
              catch _C:_E:S -> cause_of(S)
              end,
        io:format("~p cause=~s~n", [Name, Out])
    end, Cases).

cause_of(S) ->
    [Top | _] = S,
    cause_of_item(Top).

cause_of_item({_M, _F, _A, Loc}) when is_list(Loc) ->
    case [I || I = {error_info, _} <- Loc] of
        [{error_info, Map}] -> cause_value(maps:find(cause, Map));
        [] -> "no_cause_info";
        _ -> "many_info"
    end;
cause_of_item(_) -> "odd_item".

cause_value({ok, C}) when is_atom(C) -> atom_to_list(C);
cause_value({ok, _}) -> "non_atom";
cause_value(error) -> "no_cause".
"#;

/// The atoms every cause row resolves: the two cause values this crate builds, the
/// names it resolves, the fixture's own vocabulary, and the `ets` bridge's own
/// vocabulary (it answers with `true` and defaults an option list by name).
const CAUSE_SEED: [&str; 17] = [
    "id",
    "type",
    "none",
    "undefined",
    "unknown_atom_xyz",
    "my_tab",
    "tmp_tab",
    "good_name",
    "true",
    "false",
    "set",
    "protected",
    "public",
    "private",
    "named_table",
    "ordered_set",
    "keypos",
];

fn cause_seeded() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(CAUSE_SEED);
    atoms
}

/// The argument list of `case`, built on `ctx`'s heap: the deciding argument first,
/// then the `arity - 1` filler operands (which no modelled cause reads).
fn cause_args(
    ctx: &mut BifContext<'_>,
    case: &CauseCase,
    live: Eterm,
    dead: Eterm,
) -> Vec<Eterm> {
    let pid = ProcessId::new(1, 0).to_eterm();
    let atom = |ctx: &mut BifContext<'_>, name: &'static str| {
        ctx.atom(name)
            .unwrap_or_else(|error| panic!("`{name}` must be in the seeded table: {error:?}"))
    };
    let mut args: Vec<Eterm> = match case.args {
        Args::UnknownName => vec![atom(ctx, "unknown_atom_xyz")],
        Args::Integer => vec![make_small(1)],
        Args::Pid => vec![pid],
        Args::DeadTid => vec![dead],
        Args::LiveTable => vec![live],
        Args::RegisterNonAtomName => vec![make_small(1), pid],
        Args::RegisterUndefinedName => vec![atom(ctx, "undefined"), pid],
        Args::RegisterWellFormed => vec![atom(ctx, "good_name"), pid],
    };
    while (args.len() as u32) < case.arity {
        args.push(make_small(1));
    }
    assert_eq!(
        args.len() as u32,
        case.arity,
        "case `{}`: the fixture's arity and this row's disagree",
        case.name
    );
    args
}

/// The oracle's `cause=` rendering for case `name`.
fn oracle_cause(transcript: &str, name: &str) -> String {
    let prefix = format!("{name} cause=");
    transcript
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the oracle transcript has no `{name}` line:\n{transcript}"))
        .to_string()
}

impl CauseCase {
    /// The oracle's value as an atom name, when it carries one at all.
    fn oracle_by_name(&self) -> Option<&'static str> {
        match self.oracle {
            "no_cause" | "no_cause_info" => None,
            other => Some(other),
        }
    }
}

/// The whole cause matrix, cross-checked against the live oracle row by row: the
/// oracle must carry what the table claims, and [`error_cause`] must either name that
/// same value or decline.
#[test]
fn the_live_oracle_agrees_on_the_whole_cause_matrix() {
    let Some((erl, erlc)) = resolve_toolchain() else {
        skip_banner();
        return;
    };
    let scratch = Scratch::build_named(&erlc, "emcauseprobe", FIXTURE_CAUSE_ERL);
    let transcript = run_erl(&erl, scratch.path(), "emcauseprobe:run(),halt().");

    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = cause_seeded();
    let mut tables = TableRegistry::new();
    let registry = default_registry();

    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
        .with_tables(&mut tables);
    // The live table (kept) and a dead tid (created, then deleted) — the two shapes
    // `Args::LiveTable`/`Args::DeadTid` need.
    let my_tab = ctx.atom("my_tab").expect("seeded");
    let live = registry
        .call("ets", "new", 2, &mut ctx, &[my_tab, NIL])
        .expect("ets:new/2 on a seeded name");
    let tmp_tab = ctx.atom("tmp_tab").expect("seeded");
    let dead = registry
        .call("ets", "new", 2, &mut ctx, &[tmp_tab, NIL])
        .expect("ets:new/2 on a seeded name");
    registry
        .call("ets", "delete", 1, &mut ctx, &[dead])
        .expect("ets:delete/1 of the just-created table");

    for case in CAUSE_CASES {
        let oracle = oracle_cause(&transcript, case.name);
        assert_eq!(
            oracle, case.oracle,
            "case `{}`: the oracle's innermost item disagrees with this table",
            case.name
        );
        let args = cause_args(&mut ctx, case, live, dead);
        let answer = error_cause(&mut ctx, case.module, case.function, &args)
            .unwrap_or_else(|error| {
                panic!("case `{}`: error_cause failed: {error:?}", case.name)
            });
        match answer {
            ErrorCause::Undecided => assert!(
                case.expect.is_none(),
                "case `{}`: the crate declined a cause it must name (`{:?}`)",
                case.name,
                case.expect
            ),
            ErrorCause::Cause(term) => {
                let name = atoms
                    .name(atom_val(term))
                    .unwrap_or_else(|| panic!("case `{}`: the cause is not an atom", case.name));
                assert_eq!(
                    Some(name),
                    case.expect,
                    "case `{}`: the crate named a different cause",
                    case.name
                );
                assert_eq!(
                    Some(name),
                    case.oracle_by_name(),
                    "case `{}`: the crate and the oracle disagree",
                    case.name
                );
            }
        }
    }
}

/// The crate's own wiring, end to end: the BIF itself refuses an unknown table with
/// `badarg`, and [`error_cause`] names the reason the oracle's `error_info` gives.
#[test]
fn an_unknown_table_names_a_cause_end_to_end() {
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = cause_seeded();
    let mut tables = TableRegistry::new();
    let registry = default_registry();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let unknown = ctx.atom("unknown_atom_xyz").expect("seeded");
    let key = ctx.atom("id").expect("seeded");
    assert!(
        registry
            .call("ets", "lookup", 2, &mut ctx, &[unknown, key])
            .is_err(),
        "ets:lookup/2 of an unbound name must fail"
    );
    match error_cause(&mut ctx, "ets", "lookup", &[unknown, key]).expect("no failure") {
        ErrorCause::Cause(term) => assert_eq!(
            atoms.name(atom_val(term)),
            Some("id"),
            "an unbound name is `id`, not `type`"
        ),
        ErrorCause::Undecided => panic!("an unbound name is a decidable cause"),
    }
}
