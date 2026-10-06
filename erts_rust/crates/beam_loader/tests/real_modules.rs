//! A **broad real-module LOAD sweep** over the running system's OTP install.
//!
//! Where `tests/container.rs`, `tests/ir.rs` and `tests/widened_ir.rs` load a
//! couple of committed oracle fixtures, this file is the *wide* evidence: it walks
//! the real `ebin` directories of the installed OTP (`stdlib`, `kernel`, and
//! `compiler` when present) and loads **every** `*.beam` through the crate's public
//! entry point [`beam_loader::load_file`] — the whole pipeline
//! (`container::parse` → `chunk::parse` → `decode::decode_stream` →
//! `lower::build_module`).
//!
//! # What the sweep proves
//!
//! * **Load breadth.** The public loader pipeline parses an entire real OTP
//!   install; a broken walk (a missing application, a directory-resolution bug) is
//!   caught by a floor on the number of modules discovered.
//! * **Every discovered module must load.** A single module failing to parse fails
//!   the test — a load failure is a regression, never tolerated. Each failure is
//!   still printed by name with its exact typed error before the test fails.
//! * **Semantic decoding.** [`nested_fun_literals_decode_to_their_mfa`] loads two
//!   real modules whose `LitT` pools nest `EXPORT_EXT` funs inside compound
//!   literals, and asserts the decoded funs still carry their `{module, function,
//!   arity}` identity — so the sweep proves *what* decoded, not merely *how many*.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! * **A missing OTP install skips LOUDLY.** When `erl` is absent (or the
//!   `stdlib`/`kernel` `ebin` directories cannot be located) the test prints
//!   `SKIP: <reason>` and returns — it never fails in that case. It never silently
//!   passes without OTP.
//!
//! Ground truth is the system OTP install itself; the `ebin` directories are
//! located at **test time** (see [`app_ebin`]).
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader
//! ```
//!
//! With `-- --nocapture` the per-module failures and the `loaded N/M` summary are
//! printed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_codec::term::{cons_head, cons_tail, flatmap_parts, tuple_arity, tuple_element};
use beam_codec::{fun_arity_term, fun_function, fun_kind_term, fun_module, FunKind};
use beam_loader::Module;
use beam_term::prelude::{is_list, Eterm};

// ---------------------------------------------------------------------------
// Locating the installed OTP `ebin` directories
// ---------------------------------------------------------------------------

/// One application `ebin` directory to sweep, with its application name and the
/// sorted `*.beam` module basenames it holds.
struct SweepDir {
    app: &'static str,
    dir: PathBuf,
    modules: Vec<String>,
}

/// The `ebin` directory of the running Erlang's `app` application (`stdlib`,
/// `kernel`, `compiler`, …), or `None` when it cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_<APP>_DIR` (uppercased app name; accepts either the application dir
///    — `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(<app>)]),halt().'`
///    evaluated at test time (OTP's `code:lib_dir/1` returns the *application*
///    dir, whose `ebin` subdirectory holds the `.beam` files).
///
/// A caller that gets `None` must **skip loudly** (never fail) — see
/// [`skip_reason`].
fn app_ebin(app: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(format!("ERTS_{}_DIR", app.to_uppercase())) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        return has_beams(&ebin).then_some(ebin);
    }

    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let app_dir = PathBuf::from(stdout.trim());
    if has_beams(&app_dir) {
        return Some(app_dir);
    }
    let ebin = app_dir.join("ebin");
    has_beams(&ebin).then_some(ebin)
}

/// True when `dir` is a directory that contains at least one `*.beam` file.
fn has_beams(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .map(|extension| extension == "beam")
            .unwrap_or(false)
    })
}

/// The reason the test should skip (and return early), or `None` when the oracle
/// OTP install is available. `stdlib` and `kernel` are required; a caller that
/// gets `Some` must print it and return — never fail.
fn skip_reason() -> Option<String> {
    if app_ebin("stdlib").is_none() {
        return Some(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    if app_ebin("kernel").is_none() {
        return Some(
            "SKIP: system Erlang/kernel not found (need `erl` on PATH or $ERTS_KERNEL_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    None
}

/// The installed-OTP applications swept: `stdlib` and `kernel` are required
/// (guaranteed present by [`skip_reason`]); `compiler` is swept too when present
/// (extra evidence, never a failure).
fn sweep_dirs() -> Vec<SweepDir> {
    let mut dirs = Vec::new();
    for app in ["stdlib", "kernel", "compiler"] {
        let Some(dir) = app_ebin(app) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut modules: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
            .filter_map(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .collect();
        modules.sort();
        dirs.push(SweepDir { app, dir, modules });
    }
    dirs
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Loads every `*.beam` in the sweep directories through [`beam_loader::load_file`]
/// (the public pipeline) and proves each one parses to a usable module. Every load
/// failure is a hard test failure: a module that stops loading is a regression.
#[test]
fn loads_every_installed_otp_beam() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }

    let dirs = sweep_dirs();
    assert!(
        !dirs.is_empty(),
        "the sweep found no OTP `ebin` directories to load"
    );

    let mut total = 0usize;
    let mut loaded = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut empty: Vec<String> = Vec::new();

    for dir in &dirs {
        for module in &dir.modules {
            total += 1;
            let path = dir.dir.join(format!("{module}.beam"));
            match beam_loader::load_file(&path) {
                Ok(loaded_module) => {
                    loaded += 1;
                    // A successful load must have produced at least one function:
                    // an empty function list means the pipeline parsed the chunks
                    // but silently dropped the code — still a defect.
                    if loaded_module.functions.is_empty() {
                        empty.push(format!("{}:{module}", dir.app));
                    }
                }
                Err(error) => {
                    // Surface every failure loudly, with the exact typed error; the
                    // assertion below then fails the test on any of them.
                    let message = format!("{error:?}");
                    println!("LOAD-FAIL {module}: {message}");
                    failures.push((module.clone(), message));
                }
            }
        }
    }

    let mut report = String::new();
    let _ = write!(report, "SWEEP: loaded {loaded}/{total} modules");
    for dir in &dirs {
        let _ = write!(report, " ({}={})", dir.app, dir.modules.len());
    }
    println!("{report}");
    if !failures.is_empty() {
        println!("SWEEP: {} load failure(s):", failures.len());
        for (module, error) in &failures {
            println!("  - {module}: {error}");
        }
    }

    // A broad real sweep must cover a large number of modules. The floor is well
    // below the real count (stdlib+kernel ≈ 200) so an OTP point release
    // adding/removing a few modules does not break it, but a broken walk (or a
    // missing application) does.
    assert!(
        total >= 150,
        "the sweep only found {total} `.beam` files — expected stdlib+kernel (≈200)"
    );
    assert!(
        loaded == total,
        "{} of {total} installed `.beam` module(s) failed to load (a regression): {failures:?}",
        total - loaded,
    );
    assert!(
        empty.is_empty(),
        "{} module(s) loaded but yielded no function: {empty:?}",
        empty.len()
    );
}

// ---------------------------------------------------------------------------
// Semantic evidence: the nested fun literals decode to their real MFA
// ---------------------------------------------------------------------------

/// Recursively collects every fun term reachable inside `term`, descending through
/// cons spines, tuples and flat maps — the shape a `.beam` literal pool (`LitT`)
/// has. Mirrors the walker in `beam_codec/tests/ground_truth.rs`.
fn collect_funs(term: Eterm, out: &mut Vec<Eterm>) {
    if fun_kind_term(term).is_ok() {
        out.push(term);
        return;
    }
    // Walk a cons spine; the final (non-cons) tail is handled below.
    let mut cursor = term;
    while is_list(cursor) {
        collect_funs(cons_head(cursor).expect("a cons cell has a head"), out);
        cursor = cons_tail(cursor).expect("a cons cell has a tail");
    }
    if let Ok(arity) = tuple_arity(cursor) {
        for index in 0..arity {
            collect_funs(
                tuple_element(cursor, index).expect("an in-range tuple element"),
                out,
            );
        }
    } else if let Ok((keys, values)) = flatmap_parts(cursor) {
        for key in keys {
            collect_funs(*key, out);
        }
        for value in values {
            collect_funs(*value, out);
        }
    }
}

/// Every fun reachable inside a loaded module's decoded `LitT` literals.
fn literal_funs(module: &Module) -> Vec<Eterm> {
    let mut funs = Vec::new();
    for &term in module.literals().terms() {
        collect_funs(term, &mut funs);
    }
    funs
}

/// The `(module, function, arity)` names of a fun term, resolved through `module`'s
/// own atom table (its literals were decoded with that same table, so the atoms
/// resolve). `None` where the term does not carry that field.
fn fun_mfa(module: &Module, fun: Eterm) -> (Option<&str>, Option<&str>, Option<usize>) {
    (
        fun_module(fun).ok().and_then(|atom| module.atom_name(atom)),
        fun_function(fun).ok().and_then(|atom| module.atom_name(atom)),
        fun_arity_term(fun).ok(),
    )
}

/// Proves the previously-blocking fun-literal shape decodes **semantically**, not
/// just by count: two real stdlib modules whose `LitT` pools nest `EXPORT_EXT` funs
/// inside compound literals are loaded through the public pipeline, the nested funs
/// are walked out of the decoded literals, and their `{module, function, arity}`
/// identity is asserted (the oracle ground truth pinned in
/// `beam_codec/tests/ground_truth.rs`).
#[test]
fn nested_fun_literals_decode_to_their_mfa() {
    let Some(ebin) = app_ebin("stdlib") else {
        eprintln!(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the fun-literal semantic check requires the installed OTP `.beam` files"
        );
        return;
    };

    // `maps`' literal pool nests `{2, fun maps:remove/2}` — an `EXPORT_EXT` fun
    // inside a tuple inside a list.
    let maps = beam_loader::load_file(ebin.join("maps.beam"))
        .expect("maps.beam must load: its nested `fun maps:remove/2` literal must decode");
    let maps_funs = literal_funs(&maps);
    let remove = maps_funs
        .iter()
        .copied()
        .find(|&fun| fun_mfa(&maps, fun) == (Some("maps"), Some("remove"), Some(2)))
        .unwrap_or_else(|| {
            let found: Vec<_> = maps_funs.iter().map(|&fun| fun_mfa(&maps, fun)).collect();
            panic!("maps' literal pool must nest the external `fun maps:remove/2`; found {found:?}")
        });
    assert!(
        matches!(fun_kind_term(remove), Ok(FunKind::External)),
        "the nested `maps:remove` literal must decode as an external (`EXPORT_EXT`) fun"
    );

    // `gen_server`'s (larger) pool nests four `fun gen_server:format_log/N` funs
    // with arities 1 and 2, inside flat maps inside tuples/lists.
    let gen_server = beam_loader::load_file(ebin.join("gen_server.beam")).expect(
        "gen_server.beam must load: its nested `fun gen_server:format_log/N` literals must decode",
    );
    let gen_server_funs = literal_funs(&gen_server);
    assert!(
        gen_server_funs
            .iter()
            .all(|&fun| matches!(fun_kind_term(fun), Ok(FunKind::External))),
        "gen_server's pool nests only external (`EXPORT_EXT`) funs"
    );
    let mut arities: Vec<usize> = gen_server_funs
        .iter()
        .map(|&fun| fun_mfa(&gen_server, fun))
        .map(|(module, function, arity)| {
            assert_eq!(
                (module, function),
                (Some("gen_server"), Some("format_log")),
                "every nested gen_server fun is `gen_server:format_log/N`"
            );
            arity.expect("an external fun carries its arity")
        })
        .collect();
    arities.sort_unstable();
    arities.dedup();
    assert_eq!(
        arities,
        vec![1, 2],
        "gen_server's pool nests `format_log/1` and `format_log/2`"
    );
}
