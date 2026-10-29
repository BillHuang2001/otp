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
//! `erlang:term_to_binary/1` of a local (`make_fun3`) fun — this crate's half of the
//! split, pinned against the live oracle.
//!
//! A `make_fun3` fun inlines only its engine-internal descriptor
//! (`[header, uniq: u64, index: u32, env…]`, `beam_term::fun`), while the wire's
//! `NEW_FUN_EXT` (`112`) needs `arity`, the 16-byte module MD5 `uniq`, `index`,
//! `num_free`, the module atom, `old_index`, `old_uniq`, a creator pid and the `env`
//! (`external.c:3966-3993`). The split is therefore:
//!
//! * **`beam_codec`** owns the `NEW_FUN_EXT` *layout* and resolves the identity **per
//!   fun**: `Encoder::with_fun_identity_resolver` asks the installed
//!   [`beam_codec::FunIdentityResolver`] once for every `Local` fun in its own descent
//!   order and writes each answer into *that* fun's body (the single
//!   `Encoder::with_fun_identity` slot remains, for a host that knows only one).
//! * **this crate** owns deriving each identity: `src/binconv.rs::term_image` installs
//!   a resolver over the [`LocalFunIdentity`](beam_bifs::LocalFunIdentity) seam, so a
//!   term holding **two different** lambdas (a closure capturing another one, or two
//!   unrelated lambdas side by side) encodes with each fun's own
//!   `old_index`/`old_uniq`/module. A fun the seam cannot attribute keeps the codec's
//!   typed `unsupported_local_fun`, for the whole image — never a wrong one.
//!
//! The tests here pin, in order:
//!
//! 1. [`the_committed_golden_is_the_new_fun_ext_image_of_a_bare_closure`] — the exact
//!    `NEW_FUN_EXT` bytes for a fixed synthetic identity, as a **committed literal**
//!    (no oracle needed): the layout cannot drift silently.
//! 2. [`a_compiled_make_fun3_closure_encodes_byte_exactly_like_the_oracle`] — an
//!    `erlc`-compiled fixture's real `fun() -> ok end` (and the same fun inside a
//!    list, a map and a two-element list, plus a self-recursive named fun) encoded by
//!    **this crate's registry** is byte-identical to the live oracle's image, with the
//!    identity taken from the oracle's own `fun_info/2` fields. The `fun_info` items
//!    themselves (the fields `NEW_FUN_EXT` is built from) are compared field by field
//!    on the same term. With no identity (no hook, or a hook answering `None`) the
//!    answer stays the typed `unsupported_local_fun`, never a wrong image.
//! 3. [`two_lambdas_in_one_term_encode_byte_exactly_like_the_oracle`] — the shapes a
//!    one-slot encoder could not express (`f() -> G = fun() -> ok end, fun() -> G end`,
//!    whose outer fun's `env` holds the inner one, and the flat
//!    `[fun() -> ok end, fun(X) -> {X, X} end]`), each fun still carrying its own
//!    identity — byte-identical to the oracle's image.
//! 4. [`the_eval_template_closure_is_the_honest_typed_gap`] — the `-eval` route's
//!    `erl_eval` template closure, whose identity is genuinely not derivable from the
//!    frozen term.
//!
//! Measured on the host oracle (OTP 29.1.2 / erts-17.1.1): the compiled route's
//! `byte_size(term_to_binary(fun() -> ok end))` is **74** for a six-character module
//! name (the size varies with the module name, the local node name and the width of
//! `old_uniq`; the earlier "70"/"80" figures are superseded), and the `-eval` route's
//! template closure is **138**.
//!
//! Fail-loud in the crate's live-oracle style: with no usable `erl`/`erlc` the
//! oracle-backed tests print a loud banner and return — which is *not* a pass — and any
//! oracle/parse failure panics with the transcript attached.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, LocalFunIdentity};
use beam_common::Result as BeamResult;
use beam_process::ProcessId;
use beam_term::fun::FunPid;
use beam_term::{AtomTable, Eterm, NIL};

/// The fixture, compiled into a scratch directory by `erlc`.
///
/// `bare/0` is the smallest compiled local fun (arity 0, no captures); `self_rec/0` is
/// a self-recursive **named** fun (arity 1, still no captures — the recursion goes
/// through the binding, not an `env` slot); `captured/0` is the one-lambda-inside-a-
/// lambda shape whose two identities differ; `pair/0` is two unrelated lambdas (arity 0
/// and 1) side by side.
const T2BFUN_ERL: &str = r#"%% Generated by `beam_bifs/tests/term_to_binary_local_fun.rs` (never committed).
-module(t2bfun).
-export([bare/0, self_rec/0, captured/0, pair/0]).

bare() -> fun() -> ok end.
self_rec() -> fun F(0) -> 0; F(N) -> F(N - 1) end.
captured() -> G = fun() -> ok end, fun() -> G end.
pair() -> [fun() -> ok end, fun(X) -> {X, X} end].
"#;

/// The fixture module's name.
const MODULE: &str = "t2bfun";
/// The generated name of `bare/0`'s only lambda.
const BARE_NAME: &str = "-bare/0-fun-0-";

/// The committed `NEW_FUN_EXT` image of an arity-0, capture-free local fun over the
/// fixed synthetic identity `{module t2bfun, uniq [0x5A; 16], old_index 0, old_uniq 0,
/// creator <0.0.0>}`.
///
/// Field by field, from `external.c:3966-3993`: `131` `VERSION_MAGIC`, `112`
/// `NEW_FUN_EXT`, the 4-byte body size (`69` here — the tag and the size field are not
/// counted), `arity` `0`, the 16-byte module MD5 (`90` × 16), the term's own `index`
/// (`0`), `num_free` (`0`), the module atom (`119` `SMALL_ATOM_UTF8_EXT`, length 6,
/// `"t2bfun"`), `old_index` (`97` `SMALL_INTEGER_EXT` `0`), `old_uniq` (`97` `0`) and
/// the creator pid (`88` `NEW_PID_EXT`, the `"nonode@nohost"` node atom, then number,
/// serial and creation, each `0`).
///
/// The oracle's image for a **real** compiled `fun() -> ok end` has exactly this
/// layout; it differs only in the two compiler-derived fields — the 16-byte module MD5
/// and an `old_uniq` wide enough for a real hash (written as `98`
/// `INT32_EXT` + 4 bytes rather than `97` `0`, which is why its image is 74 bytes).
/// [`a_compiled_make_fun3_closure_encodes_byte_exactly_like_the_oracle`] proves that
/// against the live oracle, byte for byte.
const GOLDEN_BARE_IMAGE: &[u8] = &[
    131, 112, 0, 0, 0, 69, // VERSION_MAGIC, NEW_FUN_EXT, body size
    0, // arity
    90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, // module MD5
    0, 0, 0, 0, // index (the term's own descriptor word)
    0, 0, 0, 0, // num_free
    119, 6, 116, 50, 98, 102, 117, 110, // module atom "t2bfun"
    97, 0, // old_index
    97, 0, // old_uniq
    88, // NEW_PID_EXT
    119, 13, // node atom "nonode@nohost" (13 bytes)
    110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, //
    0, 0, 0, 0, // pid number
    0, 0, 0, 0, // pid serial
    0, 0, 0, 0, // pid creation
];

/// The `-eval` route's measured image size for `fun() -> ok end` on the host oracle —
/// an `erl_eval` template closure carrying its parsed AST in `env`.
const EVAL_TEMPLATE_IMAGE_BYTES: i64 = 138;

// ---------------------------------------------------------------------------
// The oracle toolchain
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

/// `name` in the first `$PATH` directory that holds an executable of that name.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The system `erl`/`erlc` pair — `$ERTS_ORACLE` when that names an executable, else
/// `$PATH`. `None` when either is unusable, which the tests report with a skip banner
/// instead of guessing.
fn resolve_toolchain() -> Option<(PathBuf, PathBuf)> {
    let erl = match std::env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let program = PathBuf::from(&value);
            if !is_executable(&program) {
                eprintln!(
                    "[beam_bifs term_to_binary_local_fun] $ERTS_ORACLE='{}' is not an \
                     existing executable — treating it as no oracle.",
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
fn skip_banner(what: &str) {
    eprintln!();
    eprintln!("=========== beam_bifs term_to_binary_local_fun: live oracle SKIPPED ===========");
    eprintln!("No usable `erl`/`erlc` pair found ($ERTS_ORACLE unset or not executable,");
    eprintln!("and no `erl`/`erlc` on $PATH). This test is NOT a pass — set $ERTS_ORACLE");
    eprintln!("(or install Erlang/OTP): {what} is then UNVERIFIED against the oracle.");
    eprintln!("===============================================================================");
    eprintln!();
}

/// Runs `erl -noshell [-pa <code_path>] -eval <eval>` and returns its stdout. Panics on
/// a spawn failure or a non-zero exit, so a broken oracle can never look like a pass.
fn run_erl(erl: &Path, code_path: Option<&Path>, eval: &str) -> String {
    let mut command = Command::new(erl);
    command.arg("-noshell");
    if let Some(dir) = code_path {
        command.arg("-pa").arg(dir);
    }
    let output = command
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

/// The value of the `key=value` line for `key` in an oracle transcript.
fn parse_field<'a>(output: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key}=");
    output
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the oracle output has no `{key}=` line:\n{output}"))
}

/// Parses the `key=<integer>` line for `key`.
fn parse_int(output: &str, key: &str) -> i64 {
    parse_field(output, key)
        .parse()
        .unwrap_or_else(|e| panic!("`{key}` is not an integer: {e}\n{output}"))
}

/// Parses the comma-separated integers inside a `key={…}` or `key=[…]` rendering into
/// bytes — how `binary_to_list/1` and `list_to_tuple(binary_to_list(…))` arrive.
fn parse_bytes(output: &str, key: &str) -> Vec<u8> {
    let text = parse_field(output, key);
    let inner = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| text.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')))
        .unwrap_or_else(|| panic!("`{key}={text}` is not a tuple or list rendering"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|item| {
            let n: i64 = item
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("`{item}` in `{key}={text}` is not an integer: {e}"));
            u8::try_from(n).unwrap_or_else(|_| panic!("`{item}` in `{key}={text}` is not a byte"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The scratch fixture
// ---------------------------------------------------------------------------

/// A `$TMPDIR` scratch directory holding the compiled `t2bfun` fixture.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Creates the directory, writes the fixture source and compiles it. `tag` keeps
    /// the two tests of this binary (parallel threads of one process) out of each
    /// other's directory, so one test's `Drop` cannot delete the other's fixture.
    fn build(erlc: &Path, tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "beam_bifs_term_to_binary_local_fun_{}_{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        let source = dir.join("t2bfun.erl");
        std::fs::write(&source, T2BFUN_ERL)
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
            dir.join("t2bfun.beam").is_file(),
            "`erlc` produced no t2bfun.beam in {}",
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

// ---------------------------------------------------------------------------
// The registry side (exactly how `beam_rt` drives it)
// ---------------------------------------------------------------------------

/// The atoms these cases need: the fixture's module/lambda names, the local node, the
/// `fun_info` item names and the mape key.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "undefined",
        "external",
        "local",
        "nonode@nohost",
        MODULE,
        BARE_NAME,
        "-self_rec/0-F/1-0-",
        "k",
        "arity",
        "module",
        "name",
        "new_index",
        "new_uniq",
        "index",
        "uniq",
        "env",
        "type",
        "pid",
    ]);
    atoms
}

fn make_ctx<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
    BifContext::new(heap, atoms, ProcessId::new(1, 0))
}

/// The creator pid every locally built closure carries (this node's own `<0.0.0>`).
fn creator(atoms: &AtomTable) -> FunPid {
    FunPid {
        node: atoms.lookup("nonode@nohost").expect("seeded node"),
        number: 0,
        serial: 0,
        creation: 0,
    }
}

/// A `make_fun3` (`FunKind::Local`) fun term plus the storage owning its words
/// (`[header, uniq, index, env…]`, the frozen layout;
/// `header = FUN_SUBTAG | arity << 8 | num_free << 16`).
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

/// A `LocalFunIdentity` stub answering one identity for every `Local` fun — what a
/// runtime supplies for a term whose local funs are all the same lambda.
struct StubIdentity(beam_codec::FunIdentity);

impl LocalFunIdentity for StubIdentity {
    fn local_fun_identity(
        &mut self,
        term: Eterm,
    ) -> BeamResult<Option<beam_codec::FunIdentity>> {
        if fun_kind_of(term)? != Some(beam_codec::term::FunKind::Local) {
            return Ok(None);
        }
        Ok(Some(self.0))
    }
}

/// The fun kind of `term`, or `None` when it is not a fun at all.
///
/// `beam_codec::term::fun_kind_term` answers the documented `BadArg` for a non-fun
/// (a list, a map, an atom), which is how a `LocalFunIdentity` implementation in the
/// runtime's position distinguishes "a container: no identity for *this* term" from a
/// genuine failure — the same test the runtime bridge makes before asking for a
/// `(uniq, index)` pair.
fn fun_kind_of(term: Eterm) -> BeamResult<Option<beam_codec::term::FunKind>> {
    match beam_codec::term::fun_kind_term(term) {
        Ok(kind) => Ok(Some(kind)),
        Err(beam_common::BeamError::BadArg) => Ok(None),
        Err(error) => Err(error),
    }
}

/// A `LocalFunIdentity` stub answering `None` — the defining module is not loadable
/// here (the `-eval` route, or a `NEW_FUN_EXT` image read by a VM without the module).
struct StubNone;

impl LocalFunIdentity for StubNone {
    fn local_fun_identity(
        &mut self,
        _term: Eterm,
    ) -> BeamResult<Option<beam_codec::FunIdentity>> {
        Ok(None)
    }
}

/// A `LocalFunIdentity` stub keyed by the fun's **own** descriptor `index` — the shape
/// `beam_rt`'s bridge has, since it resolves a `(uniq, index)` pair against the loaded
/// modules' `FunT` entries, so two different lambdas get two different answers.
struct IndexedIdentities {
    rows: Vec<(u32, beam_codec::FunIdentity)>,
}

impl LocalFunIdentity for IndexedIdentities {
    fn local_fun_identity(
        &mut self,
        term: Eterm,
    ) -> BeamResult<Option<beam_codec::FunIdentity>> {
        if fun_kind_of(term)? != Some(beam_codec::term::FunKind::Local) {
            return Ok(None);
        }
        let index = beam_codec::term::fun_index(term)?;
        Ok(self
            .rows
            .iter()
            .find(|(row, _)| *row == index)
            .map(|(_, identity)| *identity))
    }
}

fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    args: &[Eterm],
) -> BeamResult<Eterm> {
    reg.call("erlang", name, args.len() as u32, ctx, args)
}

/// The bytes of a `term_to_binary/1` result — a byte-aligned binary term.
fn image_bytes(image: Eterm) -> Vec<u8> {
    let (bytes, offset_bits, size_bits) =
        beam_codec::term::bitstring_bytes(image).expect("term_to_binary/1 yields a binary");
    assert_eq!(offset_bits % 8, 0, "an ETF image is byte-aligned");
    assert_eq!(size_bits % 8, 0, "an ETF image is byte-aligned");
    let start = offset_bits / 8;
    bytes[start..start + size_bits / 8].to_vec()
}

/// Encodes `term` through the registry's `term_to_binary/1`.
fn encode(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> Vec<u8> {
    let image = call(reg, ctx, "term_to_binary", &[term]).expect("term_to_binary/1");
    image_bytes(image)
}

/// `fun_info(F, Item)`'s **value** element (`element(2, …)`), or a panic naming the
/// transcript source when the item is a typed gap.
fn fun_info_value(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    item: &str,
) -> Eterm {
    let item = ctx.atom(item).expect("seeded item atom");
    let info = call(reg, ctx, "fun_info", &[fun, item]).expect("fun_info/2");
    reg.call("erlang", "element", 2, ctx, &[beam_term::make_small(2), info])
        .expect("element/2")
}

// ---------------------------------------------------------------------------
// 1. The committed golden (no oracle)
// ---------------------------------------------------------------------------

/// The exact `NEW_FUN_EXT` image, field by field, for a fixed synthetic identity.
///
/// This is the layout pin: no oracle is involved, so a change in this crate's half (the
/// identity plumbing) or in the codec's layout fails here first. The live-oracle
/// cross-check below proves the same layout is what a real compiled closure produces.
#[test]
fn the_committed_golden_is_the_new_fun_ext_image_of_a_bare_closure() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let module = atoms.lookup(MODULE).expect("seeded module");
    let identity = beam_codec::FunIdentity::named(
        module,
        [0x5A; 16],
        0,
        0,
        creator(&atoms),
        atoms.lookup(BARE_NAME).expect("seeded lambda name"),
    );
    let mut hook = StubIdentity(identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut hook);

    // arity 0, descriptor `[uniq: 0x0BAD_C0DE][index: 0]`, no captures — the oracle's
    // `fun() -> ok end` shape. The `uniq` word is engine-internal and must not appear
    // on the wire (the module MD5 is what `NEW_FUN_EXT` carries).
    let (_storage, fun) = local_fun(0, 0x0BAD_C0DE_1234_5678, 0, &[]);
    let image = encode(&reg, &mut ctx, fun);
    assert_eq!(
        image,
        GOLDEN_BARE_IMAGE,
        "the NEW_FUN_EXT image of a bare local fun changed shape"
    );

    // The layout invariants, so a future "fix" that pastes a new literal still has to
    // satisfy the format: the body size counts everything after the tag and itself, and
    // the arity byte follows it.
    assert_eq!(image.len(), 71, "the golden image's total length");
    assert_eq!(
        u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize,
        image.len() - 2,
        "the NEW_FUN_EXT size field"
    );
    assert_eq!(image[6], 0, "the arity byte");
    assert!(
        !image
            .windows(8)
            .any(|window| window == 0x0BAD_C0DE_1234_5678u64.to_be_bytes()),
        "the engine-internal descriptor `uniq` word must never reach the wire"
    );
}

// ---------------------------------------------------------------------------
// 2. The compiled-module route, byte for byte against the live oracle
// ---------------------------------------------------------------------------

/// A compiled `make_fun3` closure — bare, inside a list, inside a map, twice in a list,
/// and a self-recursive named fun — encodes to the oracle's **exact** bytes when a
/// `LocalFunIdentity` hook supplies the identity, and to the typed
/// `unsupported_local_fun` when none does.
#[test]
fn a_compiled_make_fun3_closure_encodes_byte_exactly_like_the_oracle() {
    let Some((erl, erlc)) = resolve_toolchain() else {
        skip_banner("a compiled closure's NEW_FUN_EXT bytes");
        return;
    };
    let scratch = Scratch::build(&erlc, "encode");

    // One oracle run: every image, plus the `fun_info/2` fields `NEW_FUN_EXT` is built
    // from (the same fields a `LocalFunIdentity` implementation derives). `~w` over
    // `binary_to_list/1` keeps the images machine-parseable; `~s` over
    // `atom_to_list/1` gives the module/lambda names unquoted.
    let eval = r#"
F = t2bfun:bare(),
SR = t2bfun:self_rec(),
io:format("bare=~w~n",[binary_to_list(term_to_binary(F))]),
io:format("listed=~w~n",[binary_to_list(term_to_binary([F]))]),
io:format("mapped=~w~n",[binary_to_list(term_to_binary(#{k => F}))]),
io:format("dup=~w~n",[binary_to_list(term_to_binary([F,F]))]),
io:format("selfrec=~w~n",[binary_to_list(term_to_binary(SR))]),
io:format("bare_len=~w~n",[byte_size(term_to_binary(F))]),
io:format("rt=~w~n",[binary_to_term(term_to_binary(F)) =:= F]),
io:format("mod=~s~n",[atom_to_list(element(2,erlang:fun_info(F,module)))]),
io:format("lname=~s~n",[atom_to_list(element(2,erlang:fun_info(F,name)))]),
io:format("arity=~w~n",[element(2,erlang:fun_info(F,arity))]),
io:format("new_index=~w~n",[element(2,erlang:fun_info(F,new_index))]),
io:format("index=~w~n",[element(2,erlang:fun_info(F,index))]),
io:format("uniq=~w~n",[element(2,erlang:fun_info(F,uniq))]),
io:format("new_uniq=~w~n",[list_to_tuple(binary_to_list(element(2,erlang:fun_info(F,new_uniq))))]),
io:format("env=~w~n",[list_to_tuple(element(2,erlang:fun_info(F,env)))]),
io:format("sr_arity=~w~n",[element(2,erlang:fun_info(SR,arity))]),
io:format("sr_index=~w~n",[element(2,erlang:fun_info(SR,new_index))]),
io:format("sr_uniq=~w~n",[element(2,erlang:fun_info(SR,uniq))]),
io:format("sr_name=~s~n",[atom_to_list(element(2,erlang:fun_info(SR,name)))]),
io:format("sr_env=~w~n",[list_to_tuple(element(2,erlang:fun_info(SR,env)))]),
halt().
"#;
    let oracle = run_erl(&erl, Some(scratch.path()), eval);

    // The oracle's own facts, pinned as literals so a wrong oracle cannot make this
    // test pass: a compiled closure is a `local` fun of its defining module, named
    // after the enclosing function, of the header's call arity.
    assert_eq!(parse_field(&oracle, "mod"), MODULE, "transcript:\n{oracle}");
    assert_eq!(parse_field(&oracle, "lname"), BARE_NAME, "transcript:\n{oracle}");
    assert_eq!(parse_int(&oracle, "arity"), 0, "transcript:\n{oracle}");
    assert_eq!(parse_int(&oracle, "sr_arity"), 1, "transcript:\n{oracle}");
    assert_eq!(parse_field(&oracle, "rt"), "true", "transcript:\n{oracle}");
    let bare_image = parse_bytes(&oracle, "bare");
    assert_eq!(
        bare_image.len() as i64,
        parse_int(&oracle, "bare_len"),
        "the `~w` rendering and byte_size must agree"
    );
    // The wire shape, independent of the compiler: `[131][112][size:4][arity]…`.
    assert_eq!(bare_image[0], 131, "VERSION_MAGIC");
    assert_eq!(bare_image[1], 112, "NEW_FUN_EXT");
    assert_eq!(
        u32::from_be_bytes([bare_image[2], bare_image[3], bare_image[4], bare_image[5]]) as usize,
        bare_image.len() - 2,
        "the size field counts everything after the tag and itself"
    );
    let md5 = parse_bytes(&oracle, "new_uniq");
    assert_eq!(md5.len(), 16, "new_uniq is the 16-byte module MD5");
    assert_eq!(
        &bare_image[7..23],
        md5.as_slice(),
        "the wire MD5 is the module's `new_uniq` — the identity field this crate supplies"
    );
    assert_eq!(
        parse_bytes(&oracle, "env"),
        Vec::<u8>::new(),
        "a capture-free closure has an empty env"
    );

    // --- this crate, on the equivalent frozen-layout terms ---------------------
    let reg = default_registry();
    let atoms = seeded_atoms();
    let module = atoms.lookup(MODULE).expect("seeded module");
    let mut module_md5 = [0u8; 16];
    module_md5.copy_from_slice(&md5);
    let bare_identity = beam_codec::FunIdentity::named(
        module,
        module_md5,
        parse_int(&oracle, "index") as u32,
        parse_int(&oracle, "uniq") as u32,
        creator(&atoms),
        atoms.lookup(BARE_NAME).expect("seeded lambda name"),
    );

    let mut heap = Heap::new(4096).expect("test heap");
    let mut hook = StubIdentity(bare_identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut hook);
    let bare_index = parse_int(&oracle, "new_index") as u32;
    let (_storage, fun) = local_fun(0, 0x0BAD_C0DE_1234_5678, bare_index, &[]);

    // The `fun_info/2` items the identity is built from, on the same term — the same
    // answers the oracle gave (this crate's `fun_info` is the *source* of the identity
    // a real runtime supplies, so a drift here would silently change the image).
    assert_eq!(
        signed_atom_name(&atoms, fun_info_value(&reg, &mut ctx, fun, "module")),
        MODULE
    );
    assert_eq!(
        signed_atom_name(&atoms, fun_info_value(&reg, &mut ctx, fun, "name")),
        BARE_NAME
    );
    assert_eq!(
        signed_int(fun_info_value(&reg, &mut ctx, fun, "new_index")),
        parse_int(&oracle, "new_index")
    );
    assert_eq!(
        signed_int(fun_info_value(&reg, &mut ctx, fun, "index")),
        parse_int(&oracle, "index")
    );
    assert_eq!(
        signed_int(fun_info_value(&reg, &mut ctx, fun, "uniq")),
        parse_int(&oracle, "uniq")
    );
    assert_eq!(
        image_bytes_of(fun_info_value(&reg, &mut ctx, fun, "new_uniq")),
        md5,
        "fun_info(F, new_uniq) is the wire MD5"
    );

    // The bare closure, byte for byte.
    assert_eq!(
        encode(&reg, &mut ctx, fun),
        bare_image,
        "the bare compiled closure's image differs from the oracle's"
    );
    // ... and inside the three container shapes, where the identity has to be recovered
    // from the **nested** fun (the seam answers `None` for a list/map/tuple).
    let tail = ctx.cons(fun, NIL).expect("fits");
    assert_eq!(
        encode(&reg, &mut ctx, tail),
        parse_bytes(&oracle, "listed"),
        "`[F]` differs from the oracle's image"
    );
    let key = ctx.atom("k").expect("seeded key");
    let map = beam_codec::term::make_flatmap(ctx.heap(), &[(key, fun)]).expect("a 1-pair map");
    assert_eq!(
        encode(&reg, &mut ctx, map),
        parse_bytes(&oracle, "mapped"),
        "the `#{{k => F}}` map differs from the oracle's image"
    );
    let tail = ctx.cons(fun, NIL).expect("fits");
    let twice = ctx.cons(fun, tail).expect("fits");
    assert_eq!(
        encode(&reg, &mut ctx, twice),
        parse_bytes(&oracle, "dup"),
        "`[F, F]` differs from the oracle's image"
    );

    // The self-recursive named fun: arity 1, still an empty env (the recursion is a
    // binding, not a capture), its own `index`/`old_index`.
    let self_identity = beam_codec::FunIdentity::named(
        module,
        module_md5,
        parse_int(&oracle, "sr_index") as u32,
        parse_int(&oracle, "sr_uniq") as u32,
        creator(&atoms),
        atoms.lookup("-self_rec/0-F/1-0-").expect("seeded name"),
    );
    let mut hook = StubIdentity(self_identity);
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut hook);
    let sr_index = parse_int(&oracle, "sr_index") as u32;
    let (_storage, self_rec) = local_fun(1, 0x0BAD_C0DE_1234_5678, sr_index, &[]);
    assert_eq!(
        encode(&reg, &mut ctx, self_rec),
        parse_bytes(&oracle, "selfrec"),
        "the self-recursive named fun's image differs from the oracle's"
    );
    assert_eq!(
        signed_int(fun_info_value(&reg, &mut ctx, self_rec, "arity")),
        parse_int(&oracle, "sr_arity")
    );

    // --- no identity: the typed gap, never a wrong image ----------------------
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let (_storage, fun) = local_fun(0, 0x0BAD_C0DE_1234_5678, 0, &[]);
    let err = call(&reg, &mut ctx, "term_to_binary", &[fun]).expect_err("no hook => the gap");
    assert!(
        beam_codec::is_unsupported_local_fun(&err),
        "a local fun with no identity must report the local-fun reason, got {err:?}"
    );
    let mut hook = StubNone;
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut hook);
    let (_storage, fun) = local_fun(0, 0x0BAD_C0DE_1234_5678, 0, &[]);
    let err = call(&reg, &mut ctx, "term_to_binary", &[fun]).expect_err("None => the gap");
    assert!(
        beam_codec::is_unsupported_local_fun(&err),
        "a fun the seam cannot attribute must report the local-fun reason, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Two different identities in one term — one identity per fun
// ---------------------------------------------------------------------------
/// A term holding **two different** local funs encodes, each fun with its own
/// `old_index`/`old_uniq`, and the bytes are the oracle's own:
///
/// * `t2bfun:captured/0` — `G = fun() -> ok end, fun() -> G end`: the outer fun's
///   `env` holds the inner one, i.e. two identities inside one term.
/// * `t2bfun:pair/0` — `[fun() -> ok end, fun(X) -> {X, X} end]`: two unrelated
///   lambdas, of arity 0 and 1, side by side.
///
/// This is exactly the shape a one-slot encoder could not express: it applied its one
/// `FunIdentity` to every local fun it met, so the captured lambda would have taken the
/// *outer* fun's identity — one wrong byte of a silently wrong image, which is why this
/// crate refused the whole term before the per-fun resolver landed. The resolver answers
/// by the fun's own descriptor `index`, the shape `beam_rt`'s bridge has (it resolves a
/// `(uniq, index)` pair against the loaded modules' `FunT` entries), so
/// [`IndexedIdentities`] is the whole seam here.
///
/// A fun the seam cannot attribute is still the typed `unsupported_local_fun` **for the
/// whole image**, never a partial one — pinned by `src/binconv.rs`'s unit tests, which
/// also assert each fun's body equals the body it gets when encoded alone.
#[test]
fn two_lambdas_in_one_term_encode_byte_exactly_like_the_oracle() {
    let Some((erl, erlc)) = resolve_toolchain() else {
        skip_banner("the two-lambda images (one identity per fun)");
        return;
    };
    let scratch = Scratch::build(&erlc, "mixed");
    // One oracle run: both images, each lambda's own `fun_info` fields (what the
    // resolver answers), and the inner fun's *bare* image — the body the outer fun's
    // `env` must embed verbatim.
    let eval = r#"
O = t2bfun:captured(),
[I] = element(2, erlang:fun_info(O,env)),
[L1,L2] = t2bfun:pair(),
io:format("captured=~w~n",[binary_to_list(term_to_binary(O))]),
io:format("inner=~w~n",[binary_to_list(term_to_binary(I))]),
io:format("pair=~w~n",[binary_to_list(term_to_binary([L1,L2]))]),
io:format("encoded=~w~n",[binary_to_term(term_to_binary(O)) =:= O]),
io:format("encoded_pair=~w~n",[binary_to_term(term_to_binary([L1,L2])) =:= [L1,L2]]),
io:format("o_pid=~s~n",[pid_to_list(element(2,erlang:fun_info(O,pid)))]),
io:format("i_pid=~s~n",[pid_to_list(element(2,erlang:fun_info(I,pid)))]),
io:format("o_arity=~w~n",[element(2,erlang:fun_info(O,arity))]),
io:format("i_arity=~w~n",[element(2,erlang:fun_info(I,arity))]),
io:format("o_index=~w~n",[element(2,erlang:fun_info(O,index))]),
io:format("o_new_index=~w~n",[element(2,erlang:fun_info(O,new_index))]),
io:format("o_uniq=~w~n",[element(2,erlang:fun_info(O,uniq))]),
io:format("i_index=~w~n",[element(2,erlang:fun_info(I,index))]),
io:format("i_new_index=~w~n",[element(2,erlang:fun_info(I,new_index))]),
io:format("i_uniq=~w~n",[element(2,erlang:fun_info(I,uniq))]),
io:format("l1_arity=~w~n",[element(2,erlang:fun_info(L1,arity))]),
io:format("l1_index=~w~n",[element(2,erlang:fun_info(L1,index))]),
io:format("l1_new_index=~w~n",[element(2,erlang:fun_info(L1,new_index))]),
io:format("l1_uniq=~w~n",[element(2,erlang:fun_info(L1,uniq))]),
io:format("l2_arity=~w~n",[element(2,erlang:fun_info(L2,arity))]),
io:format("l2_index=~w~n",[element(2,erlang:fun_info(L2,index))]),
io:format("l2_new_index=~w~n",[element(2,erlang:fun_info(L2,new_index))]),
io:format("l2_uniq=~w~n",[element(2,erlang:fun_info(L2,uniq))]),
io:format("md5=~w~n",[list_to_tuple(binary_to_list(element(2,erlang:fun_info(O,new_uniq))))]),
halt().
"#;
    let oracle = run_erl(&erl, Some(scratch.path()), eval);
    // --- the oracle's own facts, pinned as literals ---------------------------
    // A closure the `-eval` process built carries **its** pid as the creator, and every
    // fun in this VM was created by `<0.0.0>` — the identity [`creator`] supplies.
    assert_eq!(
        parse_field(&oracle, "o_pid"),
        "<0.0.0>",
        "the outer fun's creator pid — transcript:\n{oracle}"
    );
    assert_eq!(
        parse_field(&oracle, "i_pid"),
        "<0.0.0>",
        "the captured fun's creator pid — transcript:\n{oracle}"
    );
    assert_eq!(parse_field(&oracle, "encoded"), "true", "transcript:\n{oracle}");
    assert_eq!(
        parse_field(&oracle, "encoded_pair"),
        "true",
        "the oracle round-trips both shapes — transcript:\n{oracle}"
    );
    let outer_index = parse_int(&oracle, "o_new_index") as u32;
    let inner_index = parse_int(&oracle, "i_new_index") as u32;
    let l1_index = parse_int(&oracle, "l1_new_index") as u32;
    let l2_index = parse_int(&oracle, "l2_new_index") as u32;
    // Four distinct lambdas, so four distinct descriptor indices — a fixture whose
    // lambdas aliased would not exercise the per-fun resolver at all.
    for (a, b) in [
        (outer_index, inner_index),
        (outer_index, l1_index),
        (outer_index, l2_index),
        (inner_index, l1_index),
        (inner_index, l2_index),
        (l1_index, l2_index),
    ] {
        assert_ne!(a, b, "the fixture's lambdas must be distinct — transcript:\n{oracle}");
    }
    assert_eq!(parse_int(&oracle, "o_arity"), 0, "transcript:\n{oracle}");
    assert_eq!(parse_int(&oracle, "i_arity"), 0, "transcript:\n{oracle}");
    assert_eq!(parse_int(&oracle, "l1_arity"), 0, "transcript:\n{oracle}");
    assert_eq!(parse_int(&oracle, "l2_arity"), 1, "transcript:\n{oracle}");
    let md5 = parse_bytes(&oracle, "md5");
    assert_eq!(md5.len(), 16, "new_uniq is the 16-byte module MD5");
    let captured_image = parse_bytes(&oracle, "captured");
    let inner_image = parse_bytes(&oracle, "inner");
    let pair_image = parse_bytes(&oracle, "pair");
    // The wire shape of the capture case: one outer `NEW_FUN_EXT` whose body counts
    // everything after the tag and itself, `num_free = 1`, ending in the captured fun's
    // **own** image (not the outer's identity).
    assert_eq!((captured_image[0], captured_image[1]), (131, 112));
    assert_eq!(
        u32::from_be_bytes([
            captured_image[2],
            captured_image[3],
            captured_image[4],
            captured_image[5]
        ]) as usize,
        captured_image.len() - 2,
        "the outer NEW_FUN_EXT size field"
    );
    assert_eq!(
        u32::from_be_bytes([
            captured_image[27],
            captured_image[28],
            captured_image[29],
            captured_image[30]
        ]),
        1,
        "the outer fun captures exactly one term"
    );
    assert_eq!(
        (inner_image[0], inner_image[1]),
        (131, 112),
        "the captured fun's bare image is `VERSION_MAGIC` + `NEW_FUN_EXT`"
    );
    assert!(
        captured_image.ends_with(&inner_image[1..]),
        "the outer fun's env must embed the captured fun's own NEW_FUN_EXT body \
         (`term_to_binary(I)` minus its version byte), not the outer identity"
    );
    // The flat shape: a two-element list of two `NEW_FUN_EXT` bodies.
    assert_eq!(
        &pair_image[..6],
        &[131, 108, 0, 0, 0, 2],
        "LIST_EXT with two elements"
    );
    assert_eq!(pair_image.last(), Some(&106), "the NIL_EXT tail");
    // --- this crate, on the equivalent frozen-layout terms ---------------------
    let reg = default_registry();
    let atoms = seeded_atoms();
    let module = atoms.lookup(MODULE).expect("seeded module");
    let node = creator(&atoms);
    let mut module_md5 = [0u8; 16];
    module_md5.copy_from_slice(&md5);
    // The resolver's table: each lambda's own descriptor `index` → its own identity.
    let identity = |old_index: i64, old_uniq: i64| {
        beam_codec::FunIdentity::new(
            module,
            module_md5,
            old_index as u32,
            old_uniq as u32,
            node,
        )
    };
    let mut hook = IndexedIdentities {
        rows: vec![
            (
                inner_index,
                identity(
                    parse_int(&oracle, "i_index"),
                    parse_int(&oracle, "i_uniq"),
                ),
            ),
            (
                outer_index,
                identity(
                    parse_int(&oracle, "o_index"),
                    parse_int(&oracle, "o_uniq"),
                ),
            ),
            (
                l1_index,
                identity(
                    parse_int(&oracle, "l1_index"),
                    parse_int(&oracle, "l1_uniq"),
                ),
            ),
            (
                l2_index,
                identity(
                    parse_int(&oracle, "l2_index"),
                    parse_int(&oracle, "l2_uniq"),
                ),
            ),
        ],
    };
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut hook);
    // The capture shape: the outer fun's `env` holds the inner fun term, and the two
    // descriptor `uniq` words are engine-internal (they never reach the wire).
    let (_inner_storage, inner) = local_fun(0, 0x1111_2222_3333_4444, inner_index, &[]);
    let (_outer_storage, outer) = local_fun(0, 0x5555_6666_7777_8888, outer_index, &[inner]);
    assert_eq!(
        encode(&reg, &mut ctx, outer),
        captured_image,
        "the closure-capturing-a-lambda image differs from the oracle's"
    );
    // The flat shape: two unrelated lambdas of different arity side by side.
    let (_l1_storage, l1) = local_fun(0, 0xAAAA_AAAA_AAAA_AAAA, l1_index, &[]);
    let (_l2_storage, l2) = local_fun(1, 0xBBBB_BBBB_BBBB_BBBB, l2_index, &[]);
    let tail = ctx.cons(l2, NIL).expect("fits");
    let list = ctx.cons(l1, tail).expect("fits");
    assert_eq!(
        encode(&reg, &mut ctx, list),
        pair_image,
        "the two-lambda list image differs from the oracle's"
    );
}
// ---------------------------------------------------------------------------
// 4. The `-eval` template closure — the honest gap
// ---------------------------------------------------------------------------

/// On the `-eval` route the oracle's `fun() -> ok end` is an **`erl_eval` template
/// closure**: `{module,erl_eval}` and an identity (`uniq`, `index`) that exists only in
/// `erl_eval`'s own fun table, with the parsed AST in `env`. Nothing in the frozen term
/// carries module/`old_index`/`old_uniq`/MD5, so no seam can derive them, and this
/// runtime's evaluator stamps its closures with its own sentinel `uniq`
/// (`beam_rt/src/eval/mod.rs`'s `EVAL_FUN_UNIQ`) and an empty `env` — a different
/// identity, not the oracle's. The image therefore stays the typed
/// `unsupported_local_fun` on both the no-hook and the `None`-hook route; a `uniq` is
/// never fabricated to close the gap.
#[test]
fn the_eval_template_closure_is_the_honest_typed_gap() {
    let Some((erl, _erlc)) = resolve_toolchain() else {
        skip_banner("the -eval template closure's image and identity fields");
        return;
    };

    let eval = r#"
F = fun() -> ok end,
io:format("size=~w~n",[byte_size(term_to_binary(F))]),
io:format("mod=~s~n",[atom_to_list(element(2,erlang:fun_info(F,module)))]),
io:format("lname=~s~n",[atom_to_list(element(2,erlang:fun_info(F,name)))]),
io:format("type=~s~n",[atom_to_list(element(2,erlang:fun_info(F,type)))]),
io:format("arity=~w~n",[element(2,erlang:fun_info(F,arity))]),
io:format("new_index=~w~n",[element(2,erlang:fun_info(F,new_index))]),
io:format("env_len=~w~n",[length(element(2,erlang:fun_info(F,env)))]),
halt().
"#;
    let oracle = run_erl(&erl, None, eval);

    // The oracle facts, pinned: still a `local` fun, but owned by `erl_eval` and
    // carrying its parsed AST — which is why its image is the 138-byte template image.
    assert_eq!(parse_field(&oracle, "type"), "local", "transcript:\n{oracle}");
    assert_eq!(
        parse_field(&oracle, "mod"),
        "erl_eval",
        "the -eval closure belongs to erl_eval — transcript:\n{oracle}"
    );
    assert_eq!(parse_int(&oracle, "arity"), 0, "transcript:\n{oracle}");
    assert_eq!(
        parse_int(&oracle, "size"),
        EVAL_TEMPLATE_IMAGE_BYTES,
        "the -eval template image's measured size — transcript:\n{oracle}"
    );
    assert!(
        parse_int(&oracle, "env_len") > 0,
        "an erl_eval closure carries its AST in env — transcript:\n{oracle}"
    );

    // This crate: a local fun of the same call arity whose identity no seam can
    // attribute (here one stamped like this runtime's evaluator, with an empty env) is
    // the typed gap on both routes.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let eval_index = parse_int(&oracle, "new_index") as u32;
    // `EVAL_FUN_UNIQ` (`beam_rt/src/eval/mod.rs`): the sentinel `uniq` every closure the
    // tree-walking evaluator builds carries. Its `(uniq, index)` pair belongs to no
    // loaded module, so the bridge answers `None` — the `StubNone` case below.
    let eval_uniq = 0x6265_616d_7274_0001u64;
    let (_storage, fun) = local_fun(0, eval_uniq, eval_index, &[]);
    let mut ctx = make_ctx(&mut heap, &atoms);
    let err = call(&reg, &mut ctx, "term_to_binary", &[fun])
        .expect_err("an unattributable -eval closure is the typed gap");
    assert!(
        beam_codec::is_unsupported_local_fun(&err),
        "the -eval template closure must stay the typed gap, got {err:?}"
    );
    drop(ctx);
    let mut hook = StubNone;
    let mut ctx = make_ctx(&mut heap, &atoms).with_local_fun_identity(&mut hook);
    let (_storage, fun) = local_fun(0, eval_uniq, eval_index, &[]);
    let err = call(&reg, &mut ctx, "term_to_binary", &[fun])
        .expect_err("an unattributable -eval closure is the typed gap");
    assert!(
        beam_codec::is_unsupported_local_fun(&err),
        "the -eval template closure must stay the typed gap, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Small term readers
// ---------------------------------------------------------------------------

/// The atom name of a term that must be an atom (`fun_info`'s module/name values).
fn signed_atom_name<'a>(atoms: &'a AtomTable, term: Eterm) -> &'a str {
    let value = beam_term::atom_val(term);
    atoms
        .name(value)
        .unwrap_or_else(|| panic!("term {term:?} is not a seeded atom"))
}

/// The integer value of a term (`fun_info`'s integer values).
fn signed_int(term: Eterm) -> i64 {
    beam_term::signed_value(term)
}

/// The bytes of a binary term returned by a BIF (`fun_info(F, new_uniq)`).
fn image_bytes_of(term: Eterm) -> Vec<u8> {
    image_bytes(term)
}
