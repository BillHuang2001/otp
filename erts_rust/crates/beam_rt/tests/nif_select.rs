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

//! **`enif_select` on a real pipe fd** — the reproduction + measurement harness for the ledgered
//! contract-A.3 divergence "`enif_select` on a real pipe fd registers nothing (oracle `0`, rust
//! refusal)".
//!
//! # What is measured, and why it needs a real `.so` and the real oracle
//!
//! `enif_select` is the one `enif_*` entry point whose *whole* observable contract is a side effect
//! plus a return code: a `0` means "the registration was accepted", and the proof that it really was
//! is a `{select, Resource, Ref, ready_input}` message arriving when the descriptor becomes readable
//! (`erts/emulator/sys/common/erl_check_io.c:699-800` builds it, `:2136-2152` clears the interest as
//! it queues it, i.e. the select is **one-shot**). Neither half is visible to an in-process unit test,
//! so this harness drives a **real** C NIF library (compiled here, with the host `cc`, against the
//! host `erl_nif.h`) from an `.erl` module compiled with the host `erlc`, under **both** runtimes:
//!
//! * the built binary ([`CARGO_BIN_EXE_beam_rt`]), through the CLI's `run` action, and
//! * the system ERTS (`erl` on `$PATH`, `$ERTS_ORACLE` to override), the differential oracle.
//!
//! Both runtimes load the **same** `.so` and the same `.beam`, so the two answers are comparable
//! element for element. The fixture's own C is written here (never `beam_abi`'s
//! `tests/fixtures/test_nif.c`, which this crate does not own) and lives only in a `$TMPDIR`
//! scratch directory at run time.
//!
//! # The ledgered divergence this measures
//!
//! [`beam_abi`](beam_abi)'s `enif_select`/`enif_select_x` are complete: they reproduce the C's
//! argument handling (`e < 0` ⇒ `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT`, a null `obj` refused, a
//! null `pid` = the calling process) and then hand the request to the host hook `NifHost::select`
//! (`src/nif/enif/select.rs`). A host that does not implement the hook — `beam_rt`'s `NifHostImpl`
//! does not — gets `None` back, which the ABI maps to its own `SELECT_UNSUPPORTED`. So on this
//! runtime a genuine, ready-to-poll pipe read-end **registers nothing** and the notification never
//! arrives, while the oracle answers `0` and delivers the message.
//!
//! # Why this test is `#[ignore]`d, and what it will report once the handoffs land
//!
//! It is a **measurement of a known divergence**, not a regression pin. Two handoffs are owned by
//! other agents this round (recorded as `## The enif_select handoff` in `src/CONTEXT.md`):
//!
//! 1. a `NifHost::select` implementation on `beam_rt`'s `NifHostImpl` (`src/nif.rs`) that registers
//!    the descriptor with the runtime's one `beam_io::Poller` and keeps the resource alive, and
//! 2. a **general** delivery arm in the port layer (`src/port.rs`): today the only `enif_select`
//!    delivery is `src/port/tty.rs`'s `arm_tty_select`, hardcoded to fd `0` and `ready_input`, so a
//!    registration on an arbitrary fd has no path back to its recipient.
//!
//! When both land, the rust side's `code` flips from the refusal to `0` and its `outcome` from
//! `timeout` to `{delivered,ready_input}` — i.e. this exact harness turns green without an edit,
//! which is why it deliberately pins **no** assertion on the rust side's answer (asserting today's
//! refusal would make the eventual fix look like a regression). What *is* asserted is the oracle's
//! own answer (`0` + the delivered 4-tuple): that is the fixed reference the ledger quotes, so a move
//! there must fail loudly instead of quietly re-baselining the report.
//!
//! Run it with `-- --ignored --nocapture` (see the crate `CONTEXT.md`); without `--ignored` it is
//! inert.
//!
//! # Skip policy
//!
//! Four host facilities are needed: a C compiler (`$CC` or `cc`), the host `erl_nif.h`, `erlc`, and —
//! for the oracle half — `erl`. A missing one prints a **loud** banner and returns; it never passes
//! silently.
//!
//! # A note on the shared helpers
//!
//! [`erl_include_dir`]/[`cc`]/[`cc_available`] mirror `tests/nif.rs`'s (resolving the header is a
//! filesystem question, never "run `erl` and see"). They are duplicated rather than moved into
//! `tests/support/` because this round's file scope for this harness is this one file; the
//! de-duplication belongs with whoever next touches that shared module.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_abi::common::{ERL_NIF_SELECT_NOTSUP, ERL_NIF_SELECT_READ};

mod support;
use support::{find_on_path, oracle_eval, require_oracle, run_bounded, skip_loudly};

/// The built runtime — the `run` action is what drives the fixture.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The NIF library (written at run time, compiled with `cc -shared -fPIC`).
///
/// `arm/0` creates a **real** `pipe(2)`, wraps it in a resource of the library's own resource type,
/// arms a one-shot `ERL_NIF_SELECT_READ` on the read end through `enif_select` (with `pid == NULL`,
/// i.e. the calling process, and no `ERL_NIF_SELECT_CUSTOM_MSG`, so the C's own
/// `{select, Resource, Ref, ready_input}` shape applies) and answers
/// `{RawReturnCode, ResourceTerm, RefTerm}`. `poke/0` writes the one byte that makes the read end
/// readable, and answers the byte count.
///
/// **Why `poke/0` takes no resource term.** A resource term is resolved by
/// `enif_get_resource` through the *term word* the minting call produced: `beam_abi` keys its
/// resource registry by that word, while a NIF result is copied out onto the process heap by this
/// runtime, so the word the Erlang side holds is not the word the registry knows. That is a
/// **separate, already-documented** structural property of the ABI layer, and it must not contaminate
/// this measurement: remembering the write end in the fixture keeps the identical fixture valid on
/// both runtimes. The `enif_select` registration itself still names a **genuine** resource of the
/// library's own type (`obj`), exactly as the ABI (and the C) require — that is the operand the
/// ledgered divergence is about.
const SELECT_NIF_C: &str = r#"
#include <erl_nif.h>
#include <unistd.h>

static ErlNifResourceType* pipe_type = NULL;

typedef struct { int rd; int wr; } pipe_pair;

/* The write end of the pipe arm/0 armed last (see this file's Rust-side docs for why the write end
   is remembered here rather than carried in the resource term). */
static int armed_wr = -1;

static void pipe_dtor(ErlNifEnv* env, void* obj) {
    pipe_pair* p = (pipe_pair*) obj;
    (void) env;
    if (p->rd >= 0) { close(p->rd); p->rd = -1; }
    if (p->wr >= 0) {
        if (armed_wr == p->wr) armed_wr = -1;
        close(p->wr);
        p->wr = -1;
    }
}

static ERL_NIF_TERM arm(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int fds[2];
    int code;
    pipe_pair* p;
    ERL_NIF_TERM ref, terms[3];
    (void) argc; (void) argv;
    if (pipe(fds) != 0) return enif_make_atom(env, "pipe_failed");
    p = (pipe_pair*) enif_alloc_resource(pipe_type, sizeof(pipe_pair));
    if (p == NULL) { close(fds[0]); close(fds[1]); return enif_make_atom(env, "alloc_failed"); }
    p->rd = fds[0];
    p->wr = fds[1];
    armed_wr = p->wr;
    ref = enif_make_ref(env);
    /* The registration under measurement: a genuine resource, the read end of a real pipe,
       ERL_NIF_SELECT_READ, NULL pid (the calling process) and a plain (non-custom) message. */
    code = enif_select(env, (ErlNifEvent) p->rd, ERL_NIF_SELECT_READ, (void*) p, NULL, ref);
    terms[0] = enif_make_int(env, code);
    terms[1] = enif_make_resource(env, p);
    terms[2] = ref;
    enif_release_resource(p);
    return enif_make_tuple_from_array(env, terms, 3);
}

static ERL_NIF_TERM poke(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ssize_t n;
    (void) argc; (void) argv;
    if (armed_wr < 0) return enif_make_atom(env, "no_pipe");
    n = write(armed_wr, "x", 1);
    if (n < 0) return enif_make_atom(env, "write_failed");
    return enif_make_int(env, (int) n);
}

static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM info) {
    ErlNifResourceFlags tried;
    (void) info;
    if (pipe_type == NULL) {
        pipe_type = enif_open_resource_type(env, NULL, "select_pipe", pipe_dtor,
                                            ERL_NIF_RT_CREATE, &tried);
        if (pipe_type == NULL) return 1;
    }
    *priv = NULL;
    return 0;
}

static ErlNifFunc select_nif_funcs[] = {
    {"arm", 0, arm, 0},
    {"poke", 0, poke, 0}
};

ERL_NIF_INIT(select_nif, select_nif_funcs, load, NULL, NULL, NULL)
"#;

/// The NIF module: `init/1` loads the library, `arm/0`/`poke/0` replace their stubs.
const SELECT_NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_select.rs` (never committed).
-module(select_nif).
-export([init/1, arm/0, poke/0]).
init(Path) -> erlang:load_nif(Path, 0).
arm() -> erlang:nif_error(not_loaded).
poke() -> erlang:nif_error(not_loaded).
"#;

/// The caller: arm the read select on a fresh pipe, print the **raw** return code, write one byte
/// into the pipe, and wait (bounded) for the C's one-shot notification.
///
/// The printed `outcome` distinguishes the three answers the report needs: `{delivered, Event}` when
/// the 4-tuple arrived carrying **the very reference** the arming call passed (`Ref` is bound in the
/// head, so this clause only matches a term-equal reference), `{delivered_other_ref, Event}` when a
/// `{select, _, _, Event}` tuple arrived for a different reference, and `timeout` when nothing
/// arrived.
///
/// Every export reaches the NIF through an **external** call (`select_caller` is a different module
/// from `select_nif`), which is the dispatch point this runtime intercepts for NIFs.
const SELECT_CALLER_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_select.rs` (never committed).
-module(select_caller).
-export([probe/1]).
probe(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm(),
    io:format("code=~w~n", [Code]),
    io:format("written=~w~n", [select_nif:poke()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    ok.
wait_for_select(Ref) ->
    receive
        {select, _Resource, Ref, Event} -> {delivered, Event};
        {select, _Resource, _OtherRef, Event} -> {delivered_other_ref, Event};
        Other -> {unexpected, Other}
    after 1000 ->
        timeout
    end.
"#;

// ---------------------------------------------------------------------------------------------
// Toolchain discovery (filesystem only — never executes the tool)
// ---------------------------------------------------------------------------------------------

/// The header the include-directory search keys on.
const HEADER: &str = "erl_nif.h";

/// True when `dir` holds a readable `erl_nif.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// Canonicalize the `erl` on `$PATH` and take the sibling `include` of its `erts-*` directory.
fn from_path_erl() -> Option<PathBuf> {
    let erl = find_on_path("erl")?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// The standard installation roots to probe, `$ERL_TOP` first.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        roots.push(PathBuf::from(base));
    }
    roots
}

/// Search one installation root for an include directory holding `erl_nif.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if has_header(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    has_header(&include).then_some(include)
}

/// Locate the host `erl_nif.h` include directory **without running any Erlang program**: the sibling
/// `include` of the `erts-*` tree the `erl` on `$PATH` lives in, else a standard installation root.
fn erl_include_dir() -> Option<PathBuf> {
    from_path_erl().or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Whether a C compiler is available.
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------------

/// A compiled NIF library + its caller, in one scratch directory.
struct Fixture {
    /// The scratch directory holding the sources and the compiled artifacts.
    dir: PathBuf,
    /// The compiled `select_caller.beam` (the `run` file argument).
    caller: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc`/`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The path handed to `erlang:load_nif/2` (the `.so` **without** its suffix — the loader appends
    /// `.so`).
    fn load_path(&self) -> PathBuf {
        self.dir.join("select_nif")
    }
}

/// Compiles the `select_nif` fixture, or `None` when a host facility is missing (the caller then
/// returns early — **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        skip(&format!(
            "nif_select/{label}: no erl_nif.h include directory found"
        ));
        return None;
    };
    if !cc_available() {
        skip(&format!(
            "nif_select/{label}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    if find_on_path("erlc").is_none() {
        skip(&format!(
            "nif_select/{label}: no `erlc` on $PATH to compile the fixture module"
        ));
        return None;
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_nif_select_{label}_{}_{n}",
        std::process::id()
    ));
    let c_src = dir.join("select_nif.c");
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, SELECT_NIF_C))
    {
        skip(&format!(
            "nif_select/{label}: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    let mut erl_paths = Vec::new();
    for (stem, source) in [
        ("select_nif", SELECT_NIF_ERL),
        ("select_caller", SELECT_CALLER_ERL),
    ] {
        let path = dir.join(format!("{stem}.erl"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!(
                "nif_select/{label}: cannot write {}: {error}",
                path.display()
            ));
            return None;
        }
        erl_paths.push(path);
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/select_nif.so <dir>/select_nif.c`.
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("select_nif.so"))
        .arg(&c_src)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "nif_select/{label}: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_select/{label}: could not run the C compiler: {error}"
            ));
            return None;
        }
    }
    // The Erlang modules: `erlc -o <dir> <dir>/select_nif.erl <dir>/select_caller.erl`.
    let output = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .args(&erl_paths)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "nif_select/{label}: erlc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_select/{label}: could not run `erlc`: {error}"
            ));
            return None;
        }
    }
    let caller = dir.join("select_caller.beam");
    if !caller.is_file() {
        skip(&format!(
            "nif_select/{label}: erlc produced no {}",
            caller.display()
        ));
        return None;
    }
    Some(Fixture { dir, caller })
}

// ---------------------------------------------------------------------------------------------
// The two sides' invocations
// ---------------------------------------------------------------------------------------------

/// The `beam_rt` invocation: `-noshell -pa <dir> run <select_caller.beam> select_caller:probe/1
/// "<load-path>"`, bounded.
///
/// The load path is **double-quoted**: the `run` argument grammar parses a bare token as an atom and
/// only a quoted token as an Erlang char list, and `erlang:load_nif/2` requires a list or a binary.
/// The scratch path carries no `"` or backslash, so no escaping is needed.
fn invoke_rust(fixture: &Fixture) -> Option<String> {
    let args = vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "run".to_string(),
        fixture.caller.display().to_string(),
        "select_caller:probe/1".to_string(),
        format!("\"{}\"", fixture.load_path().display()),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run_bounded(Path::new(BINARY), &args)?;
    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    println!("--- beam_rt (exit {status}) ---");
    println!("stdout: {stdout}");
    println!("stderr: {stderr}");
    assert_eq!(
        status, 0,
        "the fixture must run on this runtime (exit {status}); stdout={stdout} stderr={stderr}"
    );
    Some(stdout)
}

/// The oracle invocation: the same `.so`/`.beam` under the system ERTS, bounded and self-terminating
/// (`oracle_eval` appends `-s init stop`).
fn invoke_oracle(fixture: &Fixture, oracle: &Path) -> Option<String> {
    let eval = format!(
        "select_caller:probe(\"{}\"), halt(0).",
        fixture.load_path().display()
    );
    let stdout = oracle_eval(oracle, Some(&fixture.dir), &eval)?;
    println!("--- oracle ({}) ---", oracle.display());
    println!("stdout: {stdout}");
    Some(stdout)
}

// ---------------------------------------------------------------------------------------------
// Reading and decoding the answers
// ---------------------------------------------------------------------------------------------

/// The value of the `key=value` line `stdout` carries, if any.
fn field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
}

/// The `code=`/`written=` value as an integer (the fixture prints `~w` of a small int).
fn int_field(stdout: &str, key: &str) -> Option<i32> {
    field(stdout, key)?.trim().parse().ok()
}

/// Decode a raw `enif_select` return code into the C's documented vocabulary (`erl_nif.h:178-183`)
/// plus this workspace's own substitution, so the report says *why* a code is what it is.
///
/// The success answer is the C's `0` (`erl_check_io.c:1464-1467`: "a plain, non-STOP registration
/// answers `0`"). The failure answers are `INT_MIN`-based bit sets; `c_int::MIN | ERL_NIF_SELECT_NOTSUP`
/// (`= -2147483520`) is the "this handle cannot be selected on" code, and `-1` is
/// `beam_abi`'s own `SELECT_UNSUPPORTED` — its negative "not serviceable" answer for a host whose
/// `NifHost::select` hook declines (module docs of `src/nif/enif/select.rs`), which is **not** one of
/// the C's codes.
fn decode(code: i32) -> String {
    if code == 0 {
        return "0 — the C's success answer: the registration was accepted".to_string();
    }
    if code == -1 {
        return "-1 — beam_abi's SELECT_UNSUPPORTED: the host's NifHost::select hook answered \
                `None` (this is not one of the C's INT_MIN-based codes)"
            .to_string();
    }
    if code == i32::MIN | ERL_NIF_SELECT_NOTSUP {
        return format!("{code} — INT_MIN | ERL_NIF_SELECT_NOTSUP ({ERL_NIF_SELECT_NOTSUP})");
    }
    if code < 0 {
        let bits = code & !i32::MIN;
        return format!("{code} — INT_MIN | {bits} (INT_MIN-based failure bitset)");
    }
    format!("{code} — a non-zero non-negative C answer")
}

// ---------------------------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------------------------

/// **Measure** `enif_select` on a real pipe read-end under both runtimes.
///
/// `#[ignore]`d on purpose: it reports the ledgered A.3 divergence, it does not pin it. The rust side
/// must answer the refusal today and the C's `0` plus a delivered `{select, Resource, Ref,
/// ready_input}` once the two handoffs described in this file's module docs land — so **no assertion
/// is made about the rust answer** (pinning today's refusal would make the fix look like a
/// regression). The oracle's own answer *is* asserted, because it is the fixed reference the ledger
/// quotes.
#[test]
#[ignore = "measurement of the ledgered A.3 enif_select divergence; run with -- --ignored --nocapture"]
fn enif_select_on_a_real_pipe_fd_measures_the_divergence() {
    let Some(fixture) = build_fixture("pipe") else {
        return;
    };
    let Some(rust_stdout) = invoke_rust(&fixture) else {
        skip_loudly(
            "nif_select",
            "the built beam_rt binary did not complete the fixture run inside the probe bound",
        );
        fixture.cleanup();
        return;
    };
    let Some((rust_code, rust_written, rust_outcome)) = answer(&rust_stdout, "beam_rt") else {
        fixture.cleanup();
        return;
    };
    let Some(oracle) = require_oracle("nif_select") else {
        fixture.cleanup();
        return;
    };
    let Some(oracle_stdout) = invoke_oracle(&fixture, &oracle) else {
        skip_loudly(
            "nif_select",
            "the oracle did not complete the probe (see the warning above)",
        );
        fixture.cleanup();
        return;
    };
    let Some((oracle_code, oracle_written, oracle_outcome)) = answer(&oracle_stdout, "oracle")
    else {
        fixture.cleanup();
        return;
    };

    // The two answers side by side — the evidence the ledger quotes.
    println!();
    println!("=============== enif_select on a real pipe fd ===============");
    println!(
        "fixture        : enif_select(env, pipe_read_fd, ERL_NIF_SELECT_READ={ERL_NIF_SELECT_READ}, \
         resource, NULL, Ref) — the same .so/.beam under both runtimes"
    );
    println!(
        "  contrast code: INT_MIN | ERL_NIF_SELECT_NOTSUP = {} (the C's 'not supported' code — not \
         what this runtime answers)",
        i32::MIN | ERL_NIF_SELECT_NOTSUP
    );
    println!(
        "beam_rt        : code={rust_code} [{}] written={rust_written} outcome={rust_outcome}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] written={oracle_written} outcome={oracle_outcome}",
        decode(oracle_code)
    );
    let agrees = rust_code == oracle_code && rust_outcome == oracle_outcome;
    println!(
        "verdict        : {}",
        if agrees {
            "AGREE — the two runtimes answer alike (the handoffs have landed; this case can now be \
             un-ignored and turned into a pin)"
        } else {
            "DIVERGENCE — this runtime does not register the select, so no notification arrives"
        }
    );
    println!("============================================================");
    println!();

    // The fixture itself is valid on both sides: the byte really was written into the pipe.
    assert_eq!(
        rust_written, 1,
        "the fixture's write into the pipe must succeed on this runtime (written={rust_written})"
    );
    assert_eq!(
        oracle_written, 1,
        "the fixture's write into the pipe must succeed on the oracle (written={oracle_written})"
    );
    // The reference the ledger quotes: the oracle accepts the registration and delivers the C's own
    // one-shot 4-tuple, carrying the very reference the arming call passed.
    assert_eq!(
        oracle_code, 0,
        "the oracle must answer the C's success code 0 for this registration, got {oracle_code} \
         [{}]",
        decode(oracle_code)
    );
    assert_eq!(
        oracle_outcome, "{delivered,ready_input}",
        "the oracle must deliver {{select, Resource, Ref, ready_input}} for the armed read"
    );
    fixture.cleanup();
}

/// Split a printed side into `(code, written, outcome)`, or `None` (loudly) when a line is missing.
fn answer(stdout: &str, side: &str) -> Option<(i32, i32, String)> {
    let code = int_field(stdout, "code");
    let written = int_field(stdout, "written");
    let outcome = field(stdout, "outcome").map(|value| value.trim().to_string());
    match (code, written, outcome) {
        (Some(code), Some(written), Some(outcome)) => Some((code, written, outcome)),
        (code, written, outcome) => {
            skip(&format!(
                "nif_select/{side}: the probe's output is incomplete (code={code:?} \
                 written={written:?} outcome={outcome:?}):\n{stdout}"
            ));
            None
        }
    }
}
