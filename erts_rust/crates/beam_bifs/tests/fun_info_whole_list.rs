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

//! Oracle cross-check for `erlang:fun_info/1` (the whole-list form), for
//! `erlang:fun_info(F, name)`, and for the `make_fun3` local-fun identity items of
//! `erlang:fun_info/2`, driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam).
//!
//! **This file is FAIL-LOUD**: it panics when no system `erl`/`erlc` oracle can be
//! run, so it can never pass vacuously. The oracle is `$ERTS_ORACLE` when that names
//! an executable, else `erl` on `$PATH` (the same resolution `tools/differential`
//! uses); `erlc` is taken from the same directory, else from `$PATH`.
//!
//! * **Case A (external funs).** `erlang:fun_info(fun lists:map/2)` is run in the
//!   oracle and the pinned five-item string
//!   `[{module,lists},{name,map},{arity,2},{env,[]},{type,external}]` is asserted;
//!   the identical `EXPORT_EXT` fun is then built here and its `/1` result compared
//!   **structurally** (`beam_term::compare::eq`, i.e. the oracle's `=:=`) against the
//!   same list. `fun_info(F, pid)` is `{pid,undefined}` for an external fun. An
//!   external *BIF* fun (`fun erlang:length/1`) is checked the same way.
//! * **Case B (compiled local funs).** A scratch `fiprobe` module
//!   (`mk/1`, `named/0`, `anon/0`) is compiled with `erlc`; the oracle prints the
//!   whole `fun_info/1` key list plus one machine-parseable `key=value` line per
//!   `fun_info` item for each closure. The equivalent `make_fun3` funs are then built
//!   here with a stub `LocalFunIdentity` supplying the oracle's `module`/MD5/
//!   `old_index`/`old_uniq`/creator **and** the lambda's local function atom (the
//!   `name` item), and every item — `name` and the full `/1` list included — is
//!   asserted to match the oracle line for line.
//! * **Case C (imported fun).** The oracle's
//!   `binary_to_term(term_to_binary(fiprobe:mk(73)))` is an `Imported`
//!   (`NEW_FUN_EXT`) fun; the same term is rebuilt here with
//!   `beam_codec::term::make_fun_imported` and its `name` (from the identity seam,
//!   since `NEW_FUN_EXT` carries no name) and full `/1` list are compared against the
//!   oracle.
//! * **The typed-gap path.** A stub `LocalFunIdentity` that answers `None` (the
//!   defining module is not loaded here) leaves `fun_info(F, name)` — and therefore
//!   the whole-list `fun_info/1` — the **typed** `unsupported_bif` gap (a raise),
//!   never a short list or a fabricated atom.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_process::ProcessId;
use beam_term::fun::FunPid;
use beam_term::{atom_val, make_small, signed_value, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// The oracle
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
/// `$PATH`. **Panics** when neither exists — this file must not pass vacuously.
fn oracle() -> PathBuf {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return program;
        }
        panic!(
            "$ERTS_ORACLE='{}' is not an executable file",
            program.display()
        );
    }
    let path = std::env::var_os("PATH")
        .expect("PATH is unset, so no `erl` oracle can be found; set $ERTS_ORACLE");
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return candidate;
        }
    }
    panic!("no `erl` oracle found: $ERTS_ORACLE is unset and `erl` is not on $PATH");
}

/// Runs `erl -noshell -eval <statement>` and returns stdout. Panics on a spawn
/// failure or a non-zero exit, so a broken oracle can never look like a pass.
fn run_erl(statement: &str) -> String {
    let program = oracle();
    let output = Command::new(&program)
        .arg("-noshell")
        .arg("-eval")
        .arg(statement)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", program.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{statement}",
        program.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Runs `erl -noshell -pa <dir> -eval <statement>` and returns stdout. Same
/// fail-loud contract as [`run_erl`].
fn run_erl_with_code_path(dir: &Path, statement: &str) -> String {
    let program = oracle();
    let output = Command::new(&program)
        .arg("-noshell")
        .arg("-pa")
        .arg(dir)
        .arg("-eval")
        .arg(statement)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", program.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{statement}",
        program.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// The system `erlc`: next to the resolved `erl`, else on `$PATH`. **Panics** when
/// neither exists (`erl` is required anyway, so the toolchain must be complete).
fn erlc() -> PathBuf {
    if let Some(dir) = oracle().parent() {
        let sibling = dir.join("erlc");
        if is_executable(&sibling) {
            return sibling;
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("erlc");
            if is_executable(&candidate) {
                return candidate;
            }
        }
    }
    panic!("no `erlc` found next to the oracle or on $PATH");
}

// ---------------------------------------------------------------------------
// Oracle output parsing
// ---------------------------------------------------------------------------

/// The value of the `key=value` line for `key` in an oracle output.
fn parse_field<'a>(output: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key}=");
    output
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the oracle output has no `{key}=` line:\n{output}"))
}

/// Parses the comma-separated integers of a `~w` inner rendering.
fn parse_ints(inner: &str) -> Vec<i64> {
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|item| {
            item.trim()
                .parse::<i64>()
                .unwrap_or_else(|e| panic!("{item} is not an integer: {e}"))
        })
        .collect()
}

/// Parses `{1,2,3}` into its integers — the shape the probe prints `env` in (a
/// tuple, so `~w` cannot render a printable list as a string).
fn parse_tuple_int_list(text: &str) -> Vec<i64> {
    let inner = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or_else(|| panic!("{text} is not a tuple rendering"));
    parse_ints(inner)
}

/// Parses `{1,2,3}` into its bytes (`binary_to_list`'s tuple rendering).
fn parse_byte_list(text: &str) -> Vec<u8> {
    parse_tuple_int_list(text)
        .into_iter()
        .map(|n| u8::try_from(n).unwrap_or_else(|_| panic!("{n} is not a byte")))
        .collect()
}

/// Parses the `keys=` line — the oracle's `fun_info/1` key names in order, printed
/// by `~w` as `[pid,module,…]` (every key is a lowercase atom, so no quoting).
fn parse_keys(text: &str) -> Vec<String> {
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{text} is not a list rendering"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner.split(',').map(|k| k.trim().to_string()).collect()
}

// ---------------------------------------------------------------------------
// Term helpers (the registry seam, exactly as `beam_rt` drives it)
// ---------------------------------------------------------------------------

/// The atoms every case below resolves: the ten `fun_info/1` keys, the produced
/// values, the module/node names and the lambda function names the oracle reports.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "undefined",
        "external",
        "local",
        "lists",
        "map",
        "reverse",
        "length",
        "erlang",
        "fiprobe",
        "nonode@nohost",
        "pid",
        "module",
        "new_index",
        "new_uniq",
        "index",
        "uniq",
        "name",
        "arity",
        "env",
        "type",
        // The lambda names the compiled `fiprobe` closures report.
        "-mk/1-fun-0-",
        "mk",
        "-anon/0-fun-0-",
    ]);
    atoms
}

fn make_ctx<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
    BifContext::new(heap, atoms, ProcessId::new(1, 0))
}

fn call(reg: &BifRegistry, ctx: &mut BifContext<'_>, name: &str, args: &[Eterm]) -> beam_common::Result<Eterm> {
    let arity = args.len() as u32;
    reg.call("erlang", name, arity, ctx, args)
}

/// The `i`-th (1-based) element of a tuple, through the registered `erlang:element/2`.
fn elem(reg: &BifRegistry, ctx: &mut BifContext<'_>, tuple: Eterm, i: i64) -> Eterm {
    reg.call("erlang", "element", 2, ctx, &[make_small(i), tuple])
        .expect("element/2")
}

/// The name of an atom term, resolved through `atoms`.
fn atom_name<'a>(atoms: &'a AtomTable, term: Eterm) -> &'a str {
    atoms
        .name(atom_val(term))
        .unwrap_or_else(|| panic!("term {term:?} is not a seeded atom"))
}

/// `fun_info(F, Item)` → the value element of the returned `{Item, Value}` pair.
fn fun_info_item(reg: &BifRegistry, ctx: &mut BifContext<'_>, fun: Eterm, item: &str) -> Eterm {
    let item = ctx.atom(item).expect("seeded item atom");
    let info = call(reg, ctx, "fun_info", &[fun, item]).expect("fun_info/2");
    elem(reg, ctx, info, 2)
}

/// Asserts the call failed with the crate's typed `unsupported_bif` raise — never a
/// wrong value and never another failure.
fn assert_unsupported_bif(result: beam_common::Result<Eterm>, what: &str) {
    match result {
        Err(ref err) if err.as_atom() == Some("unsupported_bif") => {}
        other => panic!("{what}: expected unsupported_bif, got {other:?}"),
    }
}

/// The `(key, value)` pairs of a `fun_info/1` result, in list order.
fn list_pairs(reg: &BifRegistry, ctx: &mut BifContext<'_>, list: Eterm) -> Vec<(Eterm, Eterm)> {
    let mut out = Vec::new();
    let mut cur = list;
    while cur != NIL {
        let pair = reg
            .call("erlang", "hd", 1, ctx, &[cur])
            .expect("hd/1 of a proper list");
        out.push((elem(reg, ctx, pair, 1), elem(reg, ctx, pair, 2)));
        cur = reg.call("erlang", "tl", 1, ctx, &[cur]).expect("tl/1");
    }
    out
}

/// Asserts the whole-list `fun_info/1` of `fun` has exactly the oracle's `keys=`
/// sequence (so the order and the set of surviving pairs are the oracle's) and that
/// every pair agrees with `fun_info/2` for that key — whose value each case pins
/// against the oracle's own per-item line.
fn assert_whole_list_matches(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    fun: Eterm,
    oracle_keys: &[String],
) {
    let list = call(reg, ctx, "fun_info", &[fun]).expect("fun_info/1");
    let pairs = list_pairs(reg, ctx, list);
    let got_keys: Vec<String> = pairs
        .iter()
        .map(|(key, _)| atom_name(atoms, *key).to_string())
        .collect();
    assert_eq!(got_keys.as_slice(), oracle_keys, "the /1 key sequence");
    for (key, value) in pairs {
        let two = call(reg, ctx, "fun_info", &[fun, key]).expect("fun_info/2");
        // Structural equality (`=:=`): `/1` and `/2` each build fresh boxed terms
        // (`new_uniq`'s binary, the `env` list), so raw word identity would differ.
        assert!(
            beam_term::compare::eq(value, elem(reg, ctx, two, 2), atoms),
            "`{}` disagrees between /1 and /2",
            atom_name(atoms, key)
        );
    }
}

/// Asserts the proper list `list` equals `expected` element for element.
fn assert_list(reg: &BifRegistry, ctx: &mut BifContext<'_>, list: Eterm, expected: &[i64]) {
    let mut cur = list;
    for (i, want) in expected.iter().enumerate() {
        assert_ne!(cur, NIL, "the list is shorter than expected at element {i}");
        let head = reg
            .call("erlang", "hd", 1, ctx, &[cur])
            .expect("hd/1 of a proper list");
        assert_eq!(signed_value(head), *want, "element {i}");
        cur = reg.call("erlang", "tl", 1, ctx, &[cur]).expect("tl/1");
    }
    assert_eq!(cur, NIL, "the list has extra elements after {expected:?}");
}

/// A `LocalFunIdentity` stub answering the wrapped identity (the shape a runtime
/// supplies for a loaded module's `make_fun3` closure).
struct StubIdentity(beam_codec::FunIdentity);

impl beam_bifs::LocalFunIdentity for StubIdentity {
    fn local_fun_identity(
        &mut self,
        _term: Eterm,
    ) -> beam_common::Result<Option<beam_codec::FunIdentity>> {
        Ok(Some(self.0))
    }
}

/// A `LocalFunIdentity` stub answering `None` — the defining module is not loaded
/// here, so the identity is genuinely unknown (the `-eval` template-closure case).
struct StubNone;

impl beam_bifs::LocalFunIdentity for StubNone {
    fn local_fun_identity(
        &mut self,
        _term: Eterm,
    ) -> beam_common::Result<Option<beam_codec::FunIdentity>> {
        Ok(None)
    }
}

/// The `fiprobe` identity a stub supplies for the *bare module*: the module atom
/// and a this-node creator, with the oracle's MD5/`old_index`/`old_uniq` **and** the
/// oracle's lambda `function` atom.
#[allow(clippy::too_many_arguments)]
fn fiprobe_identity(
    atoms: &AtomTable,
    md5: [u8; 16],
    old_index: u32,
    old_uniq: u32,
    function: &str,
) -> beam_codec::FunIdentity {
    beam_codec::FunIdentity::named(
        atoms.lookup("fiprobe").expect("seeded module"),
        md5,
        old_index,
        old_uniq,
        FunPid {
            node: atoms.lookup("nonode@nohost").expect("seeded node"),
            number: 0,
            serial: 0,
            creation: 0,
        },
        atoms.lookup(function).expect("seeded function atom"),
    )
}

/// A `make_fun3` (`FunKind::Local`) fun term plus the storage owning its words
/// (`[header, uniq, index, env…]`, the frozen layout).
fn local_fun(arity: u32, uniq: u64, index: u32, env: &[Eterm]) -> (Box<[Eterm]>, Eterm) {
    let header = beam_term::FUN_SUBTAG
        | ((arity as usize) << 8)
        | ((env.len() as usize) << beam_term::fun::FUN_HEADER_ENV_SIZE_OFFS);
    let mut words: Vec<Eterm> = vec![
        Eterm::from_raw(header),
        Eterm::from_raw(uniq as usize),
        Eterm::from_raw(index as usize),
    ];
    words.extend_from_slice(env);
    let words = words.into_boxed_slice();
    let term = beam_term::make_boxed(words.as_ptr());
    (words, term)
}

// ---------------------------------------------------------------------------
// Case A — an external fun (fully closed)
// ---------------------------------------------------------------------------

/// `fun_info/1` of an external fun is exactly the oracle's five-item list, and
/// `fun_info(F, pid)` is `{pid,undefined}`.
#[test]
fn an_external_fun_is_the_oracle_five_item_list() {
    // --- the oracle ---------------------------------------------------------
    let stdout = run_erl(
        r#"io:format("~p~n",[erlang:fun_info(fun lists:map/2)]), halt()."#,
    );
    assert_eq!(
        stdout.trim_end(),
        "[{module,lists},{name,map},{arity,2},{env,[]},{type,external}]"
    );

    // --- the same fun, built here -------------------------------------------
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let lists = ctx.atom("lists").expect("seeded");
    let map = ctx.atom("map").expect("seeded");
    let fun = beam_codec::term::make_fun_external(ctx.heap(), 2, atom_val(lists), atom_val(map))
        .expect("the external fun fits");

    let got = call(&reg, &mut ctx, "fun_info", &[fun]).expect("fun_info/1");

    // The oracle's list, built from the same atoms; `=:=` (structural equality).
    let module = ctx.atom("module").expect("seeded");
    let name = ctx.atom("name").expect("seeded");
    let arity = ctx.atom("arity").expect("seeded");
    let env = ctx.atom("env").expect("seeded");
    let type_ = ctx.atom("type").expect("seeded");
    let external = ctx.atom("external").expect("seeded");
    let two = ctx.int(2).expect("2 fits");
    let p0 = ctx.tuple(&[module, lists]).expect("fits");
    let p1 = ctx.tuple(&[name, map]).expect("fits");
    let p2 = ctx.tuple(&[arity, two]).expect("fits");
    let p3 = ctx.tuple(&[env, NIL]).expect("fits");
    let p4 = ctx.tuple(&[type_, external]).expect("fits");
    let expected = ctx.list(&[p0, p1, p2, p3, p4]).expect("fits");
    assert!(
        beam_term::compare::eq(got, expected, &atoms),
        "fun_info/1 is not the oracle's five-item list"
    );

    // --- fun_info(F, pid) ⇒ {pid,undefined} ---------------------------------
    let pid_item = ctx.atom("pid").expect("seeded");
    let undefined = ctx.atom("undefined").expect("seeded");
    let info = call(&reg, &mut ctx, "fun_info", &[fun, pid_item]).expect("fun_info/2 pid");
    let expected_pid = ctx.tuple(&[pid_item, undefined]).expect("fits");
    assert!(
        beam_term::compare::eq(info, expected_pid, &atoms),
        "fun_info(F, pid) of an external fun must be {{pid,undefined}}"
    );
}

// ---------------------------------------------------------------------------
// Case B/C — compiled local funs and an imported fun
// ---------------------------------------------------------------------------

/// The fixture source, written into a scratch directory and compiled with `erlc`.
const FIPROBE_ERL: &str = r#"%% Generated by `beam_bifs/tests/fun_info_whole_list.rs` (never committed).
-module(fiprobe).
-export([mk/1, named/0, anon/0]).

mk(X) -> fun(Y) -> X + Y end.
named() -> fun mk/1.
anon() -> fun(_) -> ok end.
"#;

/// The oracle script for one fun expression `fun_expr`: the whole `fun_info/1` key
/// sequence plus one `key=value` line per item.
///
/// `~w` (not `~p`) so the values never get pretty-printed across line breaks; `env`
/// is rendered through `list_to_tuple/1` so a printable list (`[73]`, which `~w`
/// would print as the string `"I"`) stays machine-parseable; and `name` through
/// `atom_to_list/1` so the lambda atom is captured **unquoted**.
fn probe(fun_expr: &str) -> String {
    format!(
        r#"
F = {fun_expr},
io:format("keys=~w~n", [[element(1, P) || P <- erlang:fun_info(F)]]),
io:format("module=~w~n", [element(2, erlang:fun_info(F, module))]),
io:format("new_index=~w~n", [element(2, erlang:fun_info(F, new_index))]),
io:format("new_uniq=~w~n", [list_to_tuple(binary_to_list(element(2, erlang:fun_info(F, new_uniq))))]),
io:format("index=~w~n", [element(2, erlang:fun_info(F, index))]),
io:format("uniq=~w~n", [element(2, erlang:fun_info(F, uniq))]),
io:format("name=~s~n", [atom_to_list(element(2, erlang:fun_info(F, name)))]),
io:format("arity=~w~n", [element(2, erlang:fun_info(F, arity))]),
io:format("env=~w~n", [list_to_tuple(element(2, erlang:fun_info(F, env)))]),
io:format("type=~w~n", [element(2, erlang:fun_info(F, type))]),
io:format("pid=~w~n", [element(2, erlang:fun_info(F, pid))]),
halt().
"#
    )
}

/// A compiled fixture in its own scratch directory.
struct Fixture {
    /// The scratch directory holding `fiprobe.erl`/`fiprobe.beam`.
    dir: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// Compiles the `fiprobe` fixture into a scratch directory unique to `tag` (the
/// cases run in parallel in one process, so they must not share a directory).
/// **Panics** on any failure: `erl` is required anyway, so a broken toolchain must
/// not look like a pass.
fn build_fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("beam_bifs_fun_info_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    let source = dir.join("fiprobe.erl");
    std::fs::write(&source, FIPROBE_ERL)
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", source.display()));
    let output = Command::new(erlc())
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `erlc`: {e}"));
    assert!(
        output.status.success(),
        "`erlc` exited with {:?}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        dir.join("fiprobe.beam").is_file(),
        "`erlc` produced no fiprobe.beam in {}",
        dir.display()
    );
    Fixture { dir }
}

/// The parsed `key=value` fields of one oracle `probe` run for a non-external fun.
struct OracleLocal {
    keys: Vec<String>,
    module: String,
    new_index: i64,
    new_uniq: Vec<u8>,
    index: i64,
    uniq: i64,
    name: String,
    arity: i64,
    env: Vec<i64>,
    type_: String,
}

/// Parses the oracle `probe` output for a fun whose identity items are all defined
/// (every `make_fun3` closure and the `Imported` fun).
fn oracle_local(output: &str) -> OracleLocal {
    OracleLocal {
        keys: parse_keys(parse_field(output, "keys")),
        module: parse_field(output, "module").to_string(),
        new_index: parse_field(output, "new_index")
            .parse()
            .expect("new_index is an integer"),
        new_uniq: parse_byte_list(parse_field(output, "new_uniq")),
        index: parse_field(output, "index")
            .parse()
            .expect("index is an integer"),
        uniq: parse_field(output, "uniq")
            .parse()
            .expect("uniq is an integer"),
        name: parse_field(output, "name").to_string(),
        arity: parse_field(output, "arity")
            .parse()
            .expect("arity is an integer"),
        env: parse_tuple_int_list(parse_field(output, "env")),
        type_: parse_field(output, "type").to_string(),
    }
}

/// Asserts every `fun_info/2` item — `name` included — and the whole-list
/// `fun_info/1` of a built `make_fun3`/`Imported` fun match the oracle's `probe`
/// fields for the same closure.
fn assert_identity_fun_matches_oracle(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    fun: Eterm,
    oracle: &OracleLocal,
) {
    assert_eq!(oracle.module, "fiprobe", "the fixture module");
    assert_eq!(oracle.type_, "local", "a compiled/imported fun is local");
    assert_eq!(oracle.new_uniq.len(), 16, "new_uniq is the 16-byte module MD5");

    assert_eq!(
        atom_name(atoms, fun_info_item(reg, ctx, fun, "module")),
        oracle.module
    );
    assert_eq!(
        signed_value(fun_info_item(reg, ctx, fun, "new_index")),
        oracle.new_index
    );

    let new_uniq = fun_info_item(reg, ctx, fun, "new_uniq");
    let (bytes, offset_bits, size_bits) =
        beam_codec::term::bitstring_bytes(new_uniq).expect("new_uniq is a binary");
    assert_eq!((offset_bits, size_bits), (0, 128));
    assert_eq!(bytes, &oracle.new_uniq[..], "new_uniq bytes");

    assert_eq!(
        signed_value(fun_info_item(reg, ctx, fun, "index")),
        oracle.index
    );
    assert_eq!(
        signed_value(fun_info_item(reg, ctx, fun, "uniq")),
        oracle.uniq
    );
    assert_eq!(
        atom_name(atoms, fun_info_item(reg, ctx, fun, "name")),
        oracle.name,
        "the lambda's local function name"
    );
    assert_eq!(
        signed_value(fun_info_item(reg, ctx, fun, "arity")),
        oracle.arity
    );

    let env_value = fun_info_item(reg, ctx, fun, "env");
    assert_list(reg, ctx, env_value, &oracle.env);

    assert_eq!(
        atom_name(atoms, fun_info_item(reg, ctx, fun, "type")),
        oracle.type_
    );

    // The creator pid is this node's own (the stub identity's `(nonode@nohost, 0)`
    // creator), i.e. the internal `<0.0.0>` immediate — matching the oracle.
    assert_eq!(
        fun_info_item(reg, ctx, fun, "pid"),
        ProcessId::new(0, 0).to_eterm()
    );

    assert_whole_list_matches(reg, ctx, atoms, fun, &oracle.keys);
}

/// `fiprobe:mk(73)` — an anonymous local closure. Every item, its `name`
/// (`'-mk/1-fun-0-'`) and the whole-list `fun_info/1` match the oracle.
#[test]
fn the_mk_closure_matches_the_oracle_item_for_item() {
    let fixture = build_fixture("mk");
    let output = run_erl_with_code_path(&fixture.dir, &probe("fiprobe:mk(73)"));
    let oracle = oracle_local(&output);
    assert_eq!(oracle.name, "-mk/1-fun-0-");

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&oracle.new_uniq);
    let identity = fiprobe_identity(
        &atoms,
        md5,
        oracle.index as u32,
        oracle.uniq as u32,
        &oracle.name,
    );
    let mut stub = StubIdentity(identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut stub);
    let env: Vec<Eterm> = oracle
        .env
        .iter()
        .map(|v| ctx.int(*v).expect("env value fits"))
        .collect();
    let (_storage, fun) = local_fun(
        oracle.arity as u32,
        0x1234_5678_9AB0,
        oracle.new_index as u32,
        &env,
    );

    assert_identity_fun_matches_oracle(&reg, &mut ctx, &atoms, fun, &oracle);
    fixture.cleanup();
}

/// `fiprobe:named()` — a **named** local fun (`fun mk/1`), whose `name` is the plain
/// atom `mk` (not a generated `-…-fun-…-`).
#[test]
fn the_named_local_fun_matches_the_oracle_item_for_item() {
    let fixture = build_fixture("named");
    let output = run_erl_with_code_path(&fixture.dir, &probe("fiprobe:named()"));
    let oracle = oracle_local(&output);
    assert_eq!(oracle.name, "mk");

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&oracle.new_uniq);
    let identity = fiprobe_identity(
        &atoms,
        md5,
        oracle.index as u32,
        oracle.uniq as u32,
        &oracle.name,
    );
    let mut stub = StubIdentity(identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut stub);
    let env: Vec<Eterm> = oracle
        .env
        .iter()
        .map(|v| ctx.int(*v).expect("env value fits"))
        .collect();
    let (_storage, fun) = local_fun(
        oracle.arity as u32,
        0x0BAD_C0DE,
        oracle.new_index as u32,
        &env,
    );

    assert_identity_fun_matches_oracle(&reg, &mut ctx, &atoms, fun, &oracle);
    fixture.cleanup();
}

/// `fiprobe:anon()` — an anonymous local closure with no captures; `name` is the
/// generated `'-anon/0-fun-0-'`.
#[test]
fn the_anon_closure_matches_the_oracle_item_for_item() {
    let fixture = build_fixture("anon");
    let output = run_erl_with_code_path(&fixture.dir, &probe("fiprobe:anon()"));
    let oracle = oracle_local(&output);
    assert_eq!(oracle.name, "-anon/0-fun-0-");

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&oracle.new_uniq);
    let identity = fiprobe_identity(
        &atoms,
        md5,
        oracle.index as u32,
        oracle.uniq as u32,
        &oracle.name,
    );
    let mut stub = StubIdentity(identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut stub);
    let env: Vec<Eterm> = oracle
        .env
        .iter()
        .map(|v| ctx.int(*v).expect("env value fits"))
        .collect();
    let (_storage, fun) = local_fun(
        oracle.arity as u32,
        0xFEED_FACE,
        oracle.new_index as u32,
        &env,
    );

    assert_identity_fun_matches_oracle(&reg, &mut ctx, &atoms, fun, &oracle);
    fixture.cleanup();
}

/// The oracle's `binary_to_term(term_to_binary(fiprobe:mk(73)))` — an `Imported`
/// (`NEW_FUN_EXT`) fun. Its identity items come from the inlined wire descriptor;
/// its `name` (`'-mk/1-fun-0-'`) from the identity seam, because the wire image
/// carries no function-name field.
#[test]
fn an_imported_fun_matches_the_oracle_item_for_item() {
    let fixture = build_fixture("imported");
    let output = run_erl_with_code_path(
        &fixture.dir,
        &probe("binary_to_term(term_to_binary(fiprobe:mk(73)))"),
    );
    let oracle = oracle_local(&output);
    assert_eq!(oracle.name, "-mk/1-fun-0-");

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&oracle.new_uniq);
    let identity = fiprobe_identity(
        &atoms,
        md5,
        oracle.index as u32,
        oracle.uniq as u32,
        &oracle.name,
    );
    let mut stub = StubIdentity(identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut stub);
    let env: Vec<Eterm> = oracle
        .env
        .iter()
        .map(|v| ctx.int(*v).expect("env value fits"))
        .collect();
    let fun = beam_codec::term::make_fun_imported(
        ctx.heap(),
        oracle.arity as usize,
        atoms.lookup("fiprobe").expect("seeded module"),
        &md5,
        oracle.new_index as u32,
        oracle.index as u32,
        oracle.uniq as u32,
        FunPid {
            node: atoms.lookup("nonode@nohost").expect("seeded node"),
            number: 0,
            serial: 0,
            creation: 0,
        },
        &env,
    )
    .expect("the imported fun fits");

    assert_identity_fun_matches_oracle(&reg, &mut ctx, &atoms, fun, &oracle);
    fixture.cleanup();
}

/// External funs — `fun lists:reverse/1` and the BIF `fun erlang:length/1` — report
/// the oracle's `{name,_}` and its five-item `fun_info/1` list.
#[test]
fn external_funs_report_the_oracle_name_and_five_item_list() {
    let cases = [
        (
            "fun lists:reverse/1",
            "lists",
            "reverse",
            "[{module,lists},{name,reverse},{arity,1},{env,[]},{type,external}]",
        ),
        (
            "fun erlang:length/1",
            "erlang",
            "length",
            "[{module,erlang},{name,length},{arity,1},{env,[]},{type,external}]",
        ),
    ];
    for (expr, module, function, pinned) in cases {
        // --- the oracle ---------------------------------------------------------
        let stdout = run_erl(&format!(
            r#"io:format("~p~n",[erlang:fun_info({expr})]), halt()."#
        ));
        assert_eq!(stdout.trim_end(), pinned, "oracle fun_info/1 for {expr}");

        // --- the same fun, built here -------------------------------------------
        let reg = default_registry();
        let atoms = seeded_atoms();
        let mut heap = Heap::new(4096).expect("test heap");
        let mut ctx = make_ctx(&mut heap, &atoms);
        let module_atom = ctx.atom(module).expect("seeded module");
        let function_atom = ctx.atom(function).expect("seeded function");
        let fun = beam_codec::term::make_fun_external(
            ctx.heap(),
            1,
            atom_val(module_atom),
            atom_val(function_atom),
        )
        .expect("the external fun fits");

        assert_eq!(
            atom_name(&atoms, fun_info_item(&reg, &mut ctx, fun, "name")),
            function,
            "the name of {expr}"
        );
        let keys: Vec<String> = ["module", "name", "arity", "env", "type"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_whole_list_matches(&reg, &mut ctx, &atoms, fun, &keys);
    }
}

/// With no identity (the defining module is not loaded here) `fun_info(F, name)` and
/// the whole-list `fun_info/1` of a `make_fun3` fun are the typed `unsupported_bif`
/// gap — a raise, never a short list or a fabricated atom.
#[test]
fn an_unresolved_local_fun_is_the_typed_gap() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut stub = StubNone;
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut stub);
    let (_storage, fun) = local_fun(1, 0x1234_5678, 3, &[]);

    assert_unsupported_bif(
        fun_info_item_result(&reg, &mut ctx, fun, "name"),
        "fun_info(F, name) of an unresolved local fun",
    );
    assert_unsupported_bif(
        call(&reg, &mut ctx, "fun_info", &[fun]),
        "fun_info/1 of an unresolved local fun",
    );
}

/// Like [`fun_info_item`] but without unwrapping, so a caller can assert the typed
/// gap.
fn fun_info_item_result(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    item: &str,
) -> beam_common::Result<Eterm> {
    let item = ctx.atom(item).expect("seeded item atom");
    call(reg, ctx, "fun_info", &[fun, item])
}
