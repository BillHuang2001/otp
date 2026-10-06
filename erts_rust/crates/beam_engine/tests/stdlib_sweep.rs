//! A **broad real-module sweep** over the running system's OTP installation.
//!
//! Where `tests/stdlib_lists.rs` and `tests/stdlib_more.rs` run a curated handful
//! of real modules, this file is the *wide* evidence: it walks the real `ebin`
//! directories of the installed OTP (`stdlib`, `kernel`, and `compiler` when
//! present) and loads **every** `*.beam` through the shared-heap / shared-atom
//! host in `support`, then runs a curated, oracle-checked spread of real exported
//! functions end to end.
//!
//! # What the sweep proves
//!
//! * **Load breadth.** `beam_loader`'s full public pipeline (container → chunk →
//!   decode → lower) parses most of a real OTP install — every module that loads
//!   yields at least one function with a non-empty label map, or the test fails.
//! * **Execution breadth / mechanism breadth.** Real functions are *run*: recursion
//!   and list building (`lists`), integer arithmetic and tuple access (`calendar`),
//!   string handling and cross-module calls (`filename` → `lists`/`erlang`,
//!   `string` → `unicode`, `ordsets` → `lists`), and tuple-backed structures
//!   (`array`).
//!
//! # Skip-vs-assert policy (explicit)
//!
//! * **Load failures are surfaced, never swallowed.** The sweep enumerates every
//!   `.beam`, asserts a loading floor, and prints the exact `(module, error)` for
//!   every module that fails to load. The observed failure set must be a **subset**
//!   of [`KNOWN_LOAD_FAILURES`] — a documented set of upstream
//!   (`beam_loader`/`beam_codec`) gaps — and each observed error must match the
//!   recorded cause. So the appearance of any *new* load failure (a regression),
//!   or the same module failing for a *different* reason, fails the test, while a
//!   listed module that starts loading simply drops off (also reported).
//! * **Execution is curated and oracle-checked.** Every expected value in the
//!   execution tests is the **real** system Erlang result, reproduced with
//!   `erl -noshell -eval …` (the commands are quoted next to each assertion). No
//!   expected value is guessed.
//! * **A blocked mechanism is skipped LOUDLY.** Where a real function still needs
//!   an unimplemented opcode/BIF, [`blocked_mechanisms_report_their_typed_error`]
//!   prints `SKIP: <module>:<f>/<n> — <typed error>` **and asserts the exact typed
//!   error**, so a later regression that turns a typed error into a *wrong result*
//!   is caught. It is never a silent pass and never a fabricated expected value.
//!
//! # Known, deliberately-not-executed hazard
//!
//! `dict:store/3` (and therefore every non-empty `dict` operation) **does not
//! terminate** under the current engine — it hangs rather than returning a typed
//! error. It is reported here and in the task report, but deliberately **not
//! executed** by any test (a hang would wedge the suite); see
//! [`dict_store_is_known_to_hang`], which asserts nothing and only documents the
//! finding.
//!
//! Ground truth is always the system `erl` (see `tests/fixtures/README.md`); the
//! stdlib/kernel directories are located at **test time** and every test skips
//! loudly when `erl`/the OTP install is unavailable.

mod support;

use std::fmt::Write as _;
use std::path::PathBuf;

use beam_common::BeamError;
use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    app_ebin, assert_atom, assert_int_list, assert_int_pair_list, assert_small, assert_term,
    int_list, skip_reason, term_tuple, ModuleHost,
};

// ---------------------------------------------------------------------------
// Locating the sweep directories
// ---------------------------------------------------------------------------

/// One application `ebin` directory to sweep, with its application name and the
/// sorted `*.beam` module basenames it holds.
struct SweepDir {
    app: &'static str,
    dir: PathBuf,
    modules: Vec<String>,
}

/// The installed-OTP applications swept: `stdlib` and `kernel` are required;
/// `compiler` is swept too when present (extra evidence, never a failure).
fn sweep_dirs() -> Vec<SweepDir> {
    let mut dirs = Vec::new();
    for app in ["stdlib", "kernel", "compiler"] {
        let Some(dir) = app_ebin(app) else {
            continue;
        };
        let mut modules: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
            .filter_map(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
            .collect();
        modules.sort();
        dirs.push(SweepDir { app, dir, modules });
    }
    dirs
}

/// Skips the calling test (loudly) when the oracle OTP install is unavailable;
/// returns `true` when the caller should return early.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// A host over the system stdlib `ebin` with `kernel`'s `ebin` on the search
/// path, so a cross-module call into `kernel` resolves by name.
fn host(dirs: &[SweepDir]) -> ModuleHost {
    let mut host = ModuleHost::new(dirs[0].dir.clone());
    for dir in &dirs[1..] {
        host.add_search_dir(dir.dir.clone());
    }
    host
}

// ---------------------------------------------------------------------------
// 1. The broad LOAD sweep
// ---------------------------------------------------------------------------

/// Why a module is known not to load today. Every cause is an upstream
/// (`beam_loader`/`beam_codec`) gap, not an engine one; the sweep still asserts
/// the *observed* error matches, so a changed cause fails the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoadFailure {
    /// An `EXPORT_EXT`/`NEW_FUN_EXT` (external fun) **literal**: `beam_codec`'s
    /// `decode_term` deliberately does not decode fun/export tags.
    FunLiteral,
    /// `lower::build_module` rejects the module with `BadArg` while lowering its
    /// instructions (the `LitT` area of these modules decodes cleanly, so the cause
    /// is the instruction model / a structure check, not the literal area).
    Lowering,
    /// An **inline big-integer instruction operand** wider than 8 magnitude bytes
    /// (`beam_loader` models no big-integer operand).
    BigOperand,
}

/// The modules of this OTP install that the loader genuinely cannot load today,
/// with the cause the sweep observes.
///
/// This is **data, not a pass**: the sweep prints every one by name and fails if
/// *any module outside this set* fails to load (a regression) or if a listed
/// module fails with a different error (a changed cause). A listed module that
/// starts loading is reported and simply drops off.
const KNOWN_LOAD_FAILURES: &[(&str, LoadFailure)] = &[
    // stdlib — external-fun literals (`EXPORT_EXT`).
    ("epp", LoadFailure::FunLiteral),
    ("erl_pp", LoadFailure::FunLiteral),
    ("erl_scan", LoadFailure::FunLiteral),
    ("erl_stdlib_errors", LoadFailure::FunLiteral),
    ("escript", LoadFailure::FunLiteral),
    ("file_sorter", LoadFailure::FunLiteral),
    ("gen_event", LoadFailure::FunLiteral),
    ("gen_fsm", LoadFailure::FunLiteral),
    ("gen_server", LoadFailure::FunLiteral),
    ("gen_statem", LoadFailure::FunLiteral),
    ("json", LoadFailure::FunLiteral),
    ("maps", LoadFailure::FunLiteral),
    ("proc_lib", LoadFailure::FunLiteral),
    ("shell", LoadFailure::FunLiteral),
    ("supervisor", LoadFailure::FunLiteral),
    ("supervisor_bridge", LoadFailure::FunLiteral),
    // stdlib — `BadArg` from `lower::build_module` (not the literal area).
    ("dets_v9", LoadFailure::Lowering),
    ("shell_docs", LoadFailure::Lowering),
    // stdlib — inline big-integer instruction operand.
    ("rand", LoadFailure::BigOperand),
    // kernel — external-fun literals.
    ("application_controller", LoadFailure::FunLiteral),
    ("erl_kernel_errors", LoadFailure::FunLiteral),
    ("error_logger", LoadFailure::FunLiteral),
    ("group", LoadFailure::FunLiteral),
    ("inet_epmd_dist", LoadFailure::FunLiteral),
    ("inet_tcp_dist", LoadFailure::FunLiteral),
    ("logger", LoadFailure::FunLiteral),
    ("logger_formatter", LoadFailure::FunLiteral),
    ("logger_h_common", LoadFailure::FunLiteral),
    ("logger_proxy", LoadFailure::FunLiteral),
    ("logger_server", LoadFailure::FunLiteral),
    // kernel — inline big-integer instruction operands.
    ("disk_log", LoadFailure::BigOperand),
    ("net", LoadFailure::BigOperand),
    ("net_kernel", LoadFailure::BigOperand),
    // compiler — external-fun literals.
    ("beam_call_types", LoadFailure::FunLiteral),
    ("beam_doc", LoadFailure::FunLiteral),
    ("beam_jump", LoadFailure::FunLiteral),
    ("beam_ssa_alias", LoadFailure::FunLiteral),
    ("beam_ssa_pre_codegen", LoadFailure::FunLiteral),
    ("beam_ssa_ss", LoadFailure::FunLiteral),
    ("compile", LoadFailure::FunLiteral),
    ("sys_core_fold", LoadFailure::FunLiteral),
];

/// Classifies a load error into the [`LoadFailure`] cause the sweep records.
fn classify(error: &BeamError) -> Option<LoadFailure> {
    match error {
        BeamError::BadArg => Some(LoadFailure::Lowering),
        BeamError::SystemLimit => Some(LoadFailure::BigOperand),
        other if format!("{other:?}").contains("unsupported_external_term") => {
            Some(LoadFailure::FunLiteral)
        }
        _ => None,
    }
}

/// The recorded cause for `module`, if it is a known load failure.
fn known_failure(module: &str) -> Option<LoadFailure> {
    KNOWN_LOAD_FAILURES
        .iter()
        .find(|(name, _)| *name == module)
        .map(|(_, cause)| *cause)
}

/// Loads every `*.beam` in the sweep directories and proves each one that loads
/// yields a usable module, while surfacing every load failure loudly.
#[test]
fn sweep_loads_every_stdlib_and_kernel_beam() {
    if skip_if_unavailable() {
        return;
    }
    let dirs = sweep_dirs();
    assert!(
        !dirs.is_empty(),
        "the sweep found no OTP `ebin` directories to load"
    );
    let mut host = host(&dirs);

    let mut total = 0usize;
    let mut loaded = 0usize;
    let mut failures: Vec<(String, BeamError)> = Vec::new();
    let mut unexpected: Vec<(String, String)> = Vec::new();
    let mut empty: Vec<String> = Vec::new();
    let mut now_loading: Vec<&str> = Vec::new();

    for dir in &dirs {
        for module in &dir.modules {
            total += 1;
            let path = dir.dir.join(format!("{module}.beam"));
            match host.load_from_path(module, path) {
                Ok(loaded_module) => {
                    loaded += 1;
                    if known_failure(module).is_some() {
                        now_loading.push(
                            KNOWN_LOAD_FAILURES
                                .iter()
                                .find(|(name, _)| *name == module)
                                .map(|(name, _)| *name)
                                .unwrap_or("?"),
                        );
                    }
                    let has_function = !loaded_module.functions.is_empty();
                    let has_labels = loaded_module
                        .functions
                        .iter()
                        .any(|function| !function.labels.is_empty());
                    if !has_function || !has_labels {
                        empty.push(format!("{}:{}", dir.app, module));
                    }
                }
                Err(error) => {
                    // Surface every failure loudly, with the exact typed error.
                    println!("LOAD-FAIL {module}: {error:?}");
                    match known_failure(module) {
                        Some(cause) if classify(&error) == Some(cause) => {
                            failures.push((module.clone(), error));
                        }
                        Some(cause) => unexpected.push((
                            module.clone(),
                            format!("known cause {cause:?}, but observed {error:?}"),
                        )),
                        None => unexpected.push((module.clone(), format!("{error:?}"))),
                    }
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
    println!(
        "SWEEP: {} known load failure(s) (documented upstream gaps):",
        failures.len()
    );
    for (module, error) in &failures {
        println!("  - {module}: {}", describe(error));
    }
    if !now_loading.is_empty() {
        println!("SWEEP: these known-failure modules now load (drop them from the list): {now_loading:?}");
    }

    // A broad real sweep must cover a large number of modules. The floor is well
    // below the real count (stdlib+kernel+compiler ≈ 260) so an OTP point release
    // adding/removing a few modules does not break it, but a broken walk (or a
    // missing application) does.
    assert!(
        total >= 150,
        "the sweep only found {total} `.beam` files — expected stdlib+kernel (≈200)"
    );
    assert!(
        loaded >= 150,
        "only {loaded}/{total} modules loaded"
    );
    assert!(
        unexpected.is_empty(),
        "{} UNEXPECTED load failure(s) (a regression or a changed cause): {unexpected:?}",
        unexpected.len()
    );
    assert!(
        empty.is_empty(),
        "{} module(s) loaded but yielded no labelled function: {empty:?}",
        empty.len()
    );
}

/// A one-line human summary of a typed load error.
fn describe(error: &BeamError) -> String {
    match error {
        BeamError::BadArg => "BadArg (lower::build_module rejected the module)".to_string(),
        BeamError::SystemLimit => {
            "SystemLimit (inline big-integer instruction operand > 8 bytes)".to_string()
        }
        other if format!("{other:?}").contains("unsupported_external_term") => {
            "unsupported_external_term (EXPORT_EXT/NEW_FUN_EXT literal)".to_string()
        }
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 2. The curated EXECUTION sweep (every expected value from the system `erl`)
// ---------------------------------------------------------------------------

/// A helper for the execution tests: a host over the sweep directories.
fn exec_host() -> ModuleHost {
    let dirs = sweep_dirs();
    host(&dirs)
}

/// Runs `module:function/arity` and returns the result (a typed error propagates).
fn run(host: &mut ModuleHost, module: &str, function: &str, args: &[Eterm]) -> Eterm {
    host.call(module, function, args)
        .unwrap_or_else(|error| panic!("{module}:{function}/{} failed: {error:?}", args.len()))
}

/// `lists` — recursion and list building over the real stdlib module.
#[test]
fn sweep_lists_recursion_and_list_building() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // oracle: lists:sum([1,2,3,4,5]) = 15
    let a = int_list(&mut host, &[1, 2, 3, 4, 5]);
    let __v = run(&mut host, "lists", "sum", &[a]);
    assert_small(&host, __v, 15);

    // oracle: lists:sort([3,1,2]) = [1,2,3]
    let a = int_list(&mut host, &[3, 1, 2]);
    let __v = run(&mut host, "lists", "sort", &[a]);
    assert_int_list(&host, __v, &[1, 2, 3]);

    // oracle: lists:usort([3,1,2,1]) = [1,2,3]
    let a = int_list(&mut host, &[3, 1, 2, 1]);
    let __v = run(&mut host, "lists", "usort", &[a]);
    assert_int_list(&host, __v, &[1, 2, 3]);

    // oracle: lists:duplicate(4, x) = [x,x,x,x]
    let n = make_small(4);
    let x = host.intern("x");
    let dup = run(&mut host, "lists", "duplicate", &[n, x]);
    let elements = terms::list_elements(dup).expect("a list");
    assert_eq!(elements.len(), 4);
    for element in elements {
        assert_atom(&host, element, "x");
    }

    // oracle: lists:flatten([[1,2],[4]]) = [1,2,4]
    let inner = int_list(&mut host, &[1, 2]);
    let rest = int_list(&mut host, &[4]);
    let nested = support::term_list(&mut host, &[inner, rest]);
    let __v = run(&mut host, "lists", "flatten", &[nested]);
    assert_int_list(&host, __v, &[1, 2, 4]);

    // oracle: lists:sublist([1,2,3,4,5],3) = [1,2,3]
    let a = int_list(&mut host, &[1, 2, 3, 4, 5]);
    let n = make_small(3);
    let __v = run(&mut host, "lists", "sublist", &[a, n]);
    assert_int_list(&host, __v, &[1, 2, 3]);

    // oracle: lists:nth(2,[1,2,3]) = 2
    let k = make_small(2);
    let a = int_list(&mut host, &[1, 2, 3]);
    let __v = run(&mut host, "lists", "nth", &[k, a]);
    assert_small(&host, __v, 2);

    // oracle: lists:last([1,2,3]) = 3
    let a = int_list(&mut host, &[1, 2, 3]);
    let __v = run(&mut host, "lists", "last", &[a]);
    assert_small(&host, __v, 3);

    // oracle: lists:max([3,1,7,2]) = 7
    let a = int_list(&mut host, &[3, 1, 7, 2]);
    let __v = run(&mut host, "lists", "max", &[a]);
    assert_small(&host, __v, 7);
}

/// `lists:zip/2` — the result is a list of freshly built 2-tuples (tuple building).
#[test]
fn sweep_lists_zip_builds_tuples() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();
    // oracle: lists:zip([1,2],[3,4]) = [{1,3},{2,4}]
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[3, 4]);
    let zipped = run(&mut host, "lists", "zip", &[a, b]);
    assert_int_pair_list(&host, zipped, &[(1, 3), (2, 4)]);
}

/// `calendar` — integer arithmetic (`rem/2`), tuple access and boolean guards.
#[test]
fn sweep_calendar_arithmetic_and_tuple_access() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // oracle: calendar:date_to_gregorian_days(2000,1,1) = 730485
    let (y, m, d) = (make_small(2000), make_small(1), make_small(1));
    let days = run(&mut host, "calendar", "date_to_gregorian_days", &[y, m, d]);
    assert_small(&host, days, 730485);

    // oracle: calendar:gregorian_days_to_date(730485) = {2000,1,1}
    let g = make_small(730485);
    let date = run(&mut host, "calendar", "gregorian_days_to_date", &[g]);
    let expected = term_tuple(&mut host, &[make_small(2000), make_small(1), make_small(1)]);
    assert_term(&host, date, expected);

    // oracle: calendar:day_of_the_week(2000,1,1) = 6   (exercises `erlang:rem/2`)
    let (y, m, d) = (make_small(2000), make_small(1), make_small(1));
    let __v = run(&mut host, "calendar", "day_of_the_week", &[y, m, d]);
    assert_small(&host, __v, 6);

    // oracle: calendar:last_day_of_the_month(2024,2) = 29
    let (y, m) = (make_small(2024), make_small(2));
    let last = run(&mut host, "calendar", "last_day_of_the_month", &[y, m]);
    assert_small(&host, last, 29);

    // oracle: calendar:is_leap_year(2024) = true
    let y = make_small(2024);
    let __v = run(&mut host, "calendar", "is_leap_year", &[y]);
    assert_atom(&host, __v, "true");

    // oracle: calendar:valid_date(2024,2,29) = true
    let (y, m, d) = (make_small(2024), make_small(2), make_small(29));
    let __v = run(&mut host, "calendar", "valid_date", &[y, m, d]);
    assert_atom(&host, __v, "true");
}

/// `filename` — strings, atoms and **cross-module calls** into `lists`/`erlang`.
#[test]
fn sweep_filename_strings_and_cross_module_calls() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // oracle: filename:basename("/a/b/c.txt") = "c.txt"
    let s = chars(&mut host, "/a/b/c.txt");
    let base = run(&mut host, "filename", "basename", &[s]);
    assert_str(&host, base, "c.txt");

    // oracle: filename:dirname("/a/b/c.txt") = "/a/b"
    let s = chars(&mut host, "/a/b/c.txt");
    let dir = run(&mut host, "filename", "dirname", &[s]);
    assert_str(&host, dir, "/a/b");

    // oracle: filename:extension("x.tar.gz") = ".gz"
    let s = chars(&mut host, "x.tar.gz");
    let ext = run(&mut host, "filename", "extension", &[s]);
    assert_str(&host, ext, ".gz");

    // oracle: filename:rootname("x.tar.gz") = "x.tar"
    let s = chars(&mut host, "x.tar.gz");
    let root = run(&mut host, "filename", "rootname", &[s]);
    assert_str(&host, root, "x.tar");

    // oracle: filename:join([a,b,c]) = "a/b/c"  (atom → string, `system_info(os_type)`)
    let a = host.intern("a");
    let b = host.intern("b");
    let c = host.intern("c");
    let parts = support::term_list(&mut host, &[a, b, c]);
    let joined = run(&mut host, "filename", "join", &[parts]);
    assert_str(&host, joined, "a/b/c");
}

/// `string` — the real string module (a cross-module `unicode` caller).
#[test]
fn sweep_string_module() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // oracle: string:uppercase("abc") = "ABC"
    let s = chars(&mut host, "abc");
    let up = run(&mut host, "string", "uppercase", &[s]);
    assert_str(&host, up, "ABC");

    // oracle: string:trim("  hi  ") = "hi"
    let s = chars(&mut host, "  hi  ");
    let trimmed = run(&mut host, "string", "trim", &[s]);
    assert_str(&host, trimmed, "hi");

    // oracle: string:reverse("abc") = "cba"
    let s = chars(&mut host, "abc");
    let reversed = run(&mut host, "string", "reverse", &[s]);
    assert_str(&host, reversed, "cba");

    // oracle: string:length("hello") = 5
    let s = chars(&mut host, "hello");
    let __v = run(&mut host, "string", "length", &[s]);
    assert_small(&host, __v, 5);
}

/// `array` — a tuple-backed structure (tuple building + element access).
#[test]
fn sweep_array_tuple_backed_structure() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();
    let list = int_list(&mut host, &[1, 2, 3]);
    let array = run(&mut host, "array", "from_list", &[list]);

    // oracle: array:size(array:from_list([1,2,3])) = 3
    let __v = run(&mut host, "array", "size", &[array]);
    assert_small(&host, __v, 3);

    // oracle: array:get(1, array:from_list([10,20,30])) = 20
    let list = int_list(&mut host, &[10, 20, 30]);
    let array = run(&mut host, "array", "from_list", &[list]);
    let one = make_small(1);
    let __v = run(&mut host, "array", "get", &[one, array]);
    assert_small(&host, __v, 20);

    // oracle: array:to_list(array:from_list([1,2,3])) = [1,2,3]
    let list = int_list(&mut host, &[1, 2, 3]);
    let array = run(&mut host, "array", "from_list", &[list]);
    let __v = run(&mut host, "array", "to_list", &[array]);
    assert_int_list(&host, __v, &[1, 2, 3]);
}

/// `ordsets` — the explicit cross-module call `ordsets:from_list/1 → lists:usort/1`.
#[test]
fn sweep_ordsets_calls_into_lists() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();
    // oracle: ordsets:from_list([3,1,2,2]) = [1,2,3]
    let a = int_list(&mut host, &[3, 1, 2, 2]);
    let __v = run(&mut host, "ordsets", "from_list", &[a]);
    assert_int_list(&host, __v, &[1, 2, 3]);
}

// ---------------------------------------------------------------------------
// 3. Blocked mechanisms (asserted loudly, never silently passed)
// ---------------------------------------------------------------------------

/// Real functions that a **documented** engine/codec gap still blocks. Each is
/// printed and its exact typed error asserted, so a regression that turns the
/// typed error into a *wrong result* is caught.
#[test]
fn blocked_mechanisms_report_their_typed_error() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // ---- maps -------------------------------------------------------------
    // The `maps` module cannot even LOAD: its literal area holds an external fun
    // (`EXPORT_EXT`), which `beam_codec::decode_term` does not decode. Every
    // map-using stdlib module (`sets`, `json`, …) is blocked transitively.
    let err = host
        .load("maps")
        .expect_err("`maps` is a known load failure (EXPORT_EXT literal)");
    println!("SKIP: maps (module load) — {err:?}");
    assert!(
        format!("{err:?}").contains("unsupported_external_term"),
        "unexpected `maps` load error: {err:?}"
    );

    // oracle: sets:new() = #{}   (loading `sets` itself works; a *non-empty* set
    // needs `maps`, so `sets:from_list/1` is the blocked case.)
    let probe = int_list(&mut host, &[1, 2, 3]);
    let err = host
        .call("sets", "from_list", &[probe])
        .expect_err("`sets:from_list/1` reaches `maps` and is blocked");
    println!("SKIP: sets:from_list/1 — {err:?}");
    assert!(
        format!("{err:?}").contains("unsupported_external_term"),
        "unexpected `sets:from_list/1` error: {err:?}"
    );

    // ---- binaries / bitstrings -------------------------------------------
    // `base64` builds its result with `bs_init_writable`, which needs a writable
    // off-heap ref-counted binary the engine does not have yet.
    let bin = terms::make_heap_binary(&mut host, &[1, 2, 3]).expect("a heap binary");
    let err = host
        .call("base64", "encode", &[bin])
        .expect_err("`base64:encode/1` needs `bs_init_writable`");
    println!("SKIP: base64:encode/1 — {err:?}");
    assert!(
        format!("{err:?}").contains("unsupported_opcode"),
        "unexpected `base64:encode/1` error: {err:?}"
    );

    // `unicode:characters_to_binary/1` is a `unicode` BIF the host floor does not
    // serve, so the real module's call fails as a typed `BadArg`.
    let s = chars(&mut host, "hi");
    let err = host
        .call("unicode", "characters_to_binary", &[s])
        .expect_err("`unicode:characters_to_binary/1` is not in the host floor");
    println!("SKIP: unicode:characters_to_binary/1 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `unicode:characters_to_binary/1` error: {err:?}"
    );
}

/// Documents a **non-termination hazard** found by the sweep.
///
/// `dict:new/0` runs fine, but `dict:store/3` (and therefore any non-empty `dict`
/// operation) never returns under the current engine — it loops rather than
/// raising. This test does **not** execute it (a hang would wedge the suite); it
/// only records the finding so it is not lost.
#[test]
fn dict_store_is_known_to_hang() {
    if skip_if_unavailable() {
        return;
    }
    println!(
        "KNOWN HAZARD: dict:store/3 does not terminate under the current engine \
         (dict:new/0 returns fine). Deliberately not executed here."
    );
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A string (an Erlang list of character code points) as a term.
fn chars(host: &mut ModuleHost, text: &str) -> Eterm {
    int_list(host, &text.bytes().map(i64::from).collect::<Vec<_>>())
}

/// Asserts `got` is the Erlang string `want` (a list of character code points).
fn assert_str(host: &ModuleHost, got: Eterm, want: &str) {
    assert_int_list(host, got, &want.bytes().map(i64::from).collect::<Vec<_>>());
}
