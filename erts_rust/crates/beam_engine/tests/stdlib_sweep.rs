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
//!   decode → lower) parses the *whole* of a real OTP install: **every** discovered
//!   module loads and yields at least one function with a non-empty label map, or
//!   the test fails.
//! * **Execution breadth / mechanism breadth.** Real functions are *run*: recursion
//!   and list building (`lists`), integer arithmetic and tuple access (`calendar`),
//!   string handling and cross-module calls (`filename` → `lists`/`erlang`,
//!   `string` → `unicode`, `ordsets` → `lists`), tuple-backed structures (`array`),
//!   and functions of modules that only started loading once the fun-literal decode
//!   landed (`maps`, `erl_scan`).
//!
//! # Skip-vs-assert policy (explicit)
//!
//! * **Every discovered module must load — a load failure FAILS the test.** The
//!   sweep enumerates every `.beam`, asserts a breadth floor, and requires that
//!   **all** of them load. Any module that fails to load is printed loudly with its
//!   exact typed error and *fails the test*; the failure is never swallowed and
//!   never silently allowed. (A loading module must also yield at least one function
//!   with a non-empty label map, or the test fails too.)
//! * **Execution is curated and oracle-checked.** Every expected value in the
//!   execution tests is the **real** system Erlang result, reproduced with
//!   `erl -noshell -eval …` (the commands are quoted next to each assertion). No
//!   expected value is guessed.
//! * **A blocked mechanism is skipped LOUDLY.** Where a real function still needs
//!   an unimplemented BIF/mechanism, [`blocked_mechanisms_report_their_typed_error`]
//!   prints `SKIP: <module>:<f>/<n> — <typed error>` **and asserts the exact typed
//!   error**, so a later regression that turns a typed error into a *wrong result*
//!   is caught. It is never a silent pass and never a fabricated expected value.
//!
//! Ground truth is always the system `erl` (see `tests/fixtures/README.md`); the
//! stdlib/kernel directories are located at **test time** and every test skips
//! loudly when `erl`/the OTP install is unavailable.

mod support;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{Function, Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{make_small, Atom, Eterm};
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

/// Loads every `*.beam` in the sweep directories and proves that **all** of them
/// load into a usable module — surfacing (and failing on) any load failure loudly.
///
/// The load breadth is now complete: `beam_codec` decodes the whole fun-literal
/// surface (`EXPORT_EXT`/`NEW_FUN_EXT`), the last blocker, so the sweep requires
/// **every** discovered module to load (and to yield at least one labelled
/// function). A regression is caught twice over: the exact failure is printed with
/// its typed error *and* the test fails.
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
    let mut empty: Vec<String> = Vec::new();

    for dir in &dirs {
        for module in &dir.modules {
            total += 1;
            let path = dir.dir.join(format!("{module}.beam"));
            match host.load_from_path(module, path) {
                Ok(loaded_module) => {
                    loaded += 1;
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
                    // Surface every failure loudly, with the exact typed error — and
                    // fail: a module that does not load is a regression, never a pass.
                    println!("LOAD-FAIL {module}: {error:?}");
                    failures.push((module.clone(), error));
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

    // A broad real sweep must cover a large number of modules. The floor is well
    // below the real count (stdlib+kernel+compiler ≈ 260) so an OTP point release
    // adding/removing a few modules does not break it, but a broken walk (or a
    // missing application) does.
    assert!(
        total >= 150,
        "the sweep only found {total} `.beam` files — expected stdlib+kernel (≈200)"
    );
    assert_eq!(
        loaded,
        total,
        "{} of {total} discovered modules failed to LOAD — every discovered module \
         must load (the failures above carry their exact typed errors): {failures:?}",
        failures.len()
    );
    assert!(
        empty.is_empty(),
        "{} module(s) loaded but yielded no labelled function: {empty:?}",
        empty.len()
    );
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

/// `dict` — a **non-empty** dictionary, i.e. the closure path that carries
/// **captured variables**.
///
/// `dict:store/3` hands `on_bucket/3` an anonymous fun that closes over both the
/// key and the value (`fun (B0) -> store_bkt_val(Key, Val, B0) end`). Running it
/// therefore exercises `make_fun3` **with** free variables plus `call_fun2`: the
/// fun-application seam must place the call arguments in `x(0..arity)` and the
/// closure's free variables at `x(arity..)` — the C `call_fun` order
/// (`reg[i + arity] = funp->env[i]`, `../../erts/emulator/beam/beam_common.c:1631`).
/// A fun with *no* free variables (the `lists:map` harness funs) can never catch
/// a reversed order, so this is the covering case.
#[test]
fn sweep_dict_store_and_lookup() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // oracle: dict:to_list(dict:store(a,1,dict:new())) = [{a,1}]
    let d0 = run(&mut host, "dict", "new", &[]);
    let a = host.intern("a");
    let one = make_small(1);
    let d1 = run(&mut host, "dict", "store", &[a, one, d0]);
    let list = run(&mut host, "dict", "to_list", &[d1]);
    let elements = terms::list_elements(list).expect("dict:to_list is a proper list");
    assert_eq!(elements.len(), 1, "a one-key dict: {elements:?}");
    let expected = term_tuple(&mut host, &[a, one]);
    assert_term(&host, elements[0], expected);

    // oracle: D2 = dict:store(b,2,D1) gives
    //   dict:fetch(a,D2)=1, dict:fetch(b,D2)=2, dict:size(D2)=2,
    //   dict:is_key(b,D2)=true, dict:find(zz,D2)=error
    let b = host.intern("b");
    let d2 = run(&mut host, "dict", "store", &[b, make_small(2), d1]);
    let v = run(&mut host, "dict", "fetch", &[a, d2]);
    assert_small(&host, v, 1);
    let v = run(&mut host, "dict", "fetch", &[b, d2]);
    assert_small(&host, v, 2);
    let n = run(&mut host, "dict", "size", &[d2]);
    assert_small(&host, n, 2);
    let k = run(&mut host, "dict", "is_key", &[b, d2]);
    assert_atom(&host, k, "true");
    let zz = host.intern("zz");
    let miss = run(&mut host, "dict", "find", &[zz, d2]);
    assert_atom(&host, miss, "error");

    // oracle: dict:fetch(a, dict:store(a,99,D2)) = 99 and dict:size(...) = 2
    //   (an existing key is overwritten, not added).
    let d3 = run(&mut host, "dict", "store", &[a, make_small(99), d2]);
    let v = run(&mut host, "dict", "fetch", &[a, d3]);
    assert_small(&host, v, 99);
    let n = run(&mut host, "dict", "size", &[d3]);
    assert_small(&host, n, 2);
}

/// `dict` — inserting past the expansion threshold, which drives the segment
/// growth/rehash path (`maybe_expand/2` → `maybe_expand_segs`/`rehash/4`) as well
/// as the same captured-variable closure.
#[test]
fn sweep_dict_grows_past_the_expansion_threshold() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // oracle: storing K => K*10 for K = 1..=81 in a fresh dict gives
    //   dict:size(D) = 81 and lists:sort(dict:to_list(D)) =
    //   [{1,10},{2,20},…,{81,810}]
    let mut dict = run(&mut host, "dict", "new", &[]);
    for key in 1..=81i64 {
        dict = run(
            &mut host,
            "dict",
            "store",
            &[make_small(key), make_small(key * 10), dict],
        );
    }
    let n = run(&mut host, "dict", "size", &[dict]);
    assert_small(&host, n, 81);

    let list = run(&mut host, "dict", "to_list", &[dict]);
    let sorted = run(&mut host, "lists", "sort", &[list]);
    let elements = terms::list_elements(sorted).expect("a proper list");
    assert_eq!(elements.len(), 81);
    for (index, &pair) in elements.iter().enumerate() {
        let key = index as i64 + 1;
        assert_eq!(terms::tuple_arity(pair).ok(), Some(2), "pair {index}: {pair:?}");
        assert_small(&host, terms::tuple_element(pair, 0).expect("key"), key);
        assert_small(&host, terms::tuple_element(pair, 1).expect("value"), key * 10);
    }
}

// ---------------------------------------------------------------------------
// 3. Off-heap (refc) binary construction: `bs_init_writable` + `private_append`
// ---------------------------------------------------------------------------

/// `base64:encode/1` end to end — the canonical **off-heap refc binary** builder.
///
/// The real `base64` module opens an accumulator with `bs_init_writable` and
/// extends it with a `bs_create_bin` `private_append` segment on every loop
/// iteration, so this test drives exactly the two opcode forms the engine gained
/// (and the `> ERL_ONHEAP_BINARY_LIMIT` result representation, since a 60-byte
/// input encodes to 80 bytes).
///
/// The expected bytes are the system `erl` oracle verbatim:
///
/// ```text
/// erl -noshell -eval 'io:format("~p~n",
///     [base64:encode(list_to_binary(lists:seq(0,59)))]),halt().'
/// <<"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7">>
/// ```
#[test]
fn sweep_base64_builds_an_off_heap_binary() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // The input `list_to_binary(lists:seq(0,59))`: bytes 0..59, built test-side
    // (60 bytes is still an on-heap binary; the *result* is the refc one).
    let input: Vec<u8> = (0u8..60).collect();
    let bin = terms::make_heap_binary(&mut host, &input).expect("a 60-byte heap binary");
    let got = run(&mut host, "base64", "encode", &[bin]);
    let bytes = terms::binary_bytes(got).expect("base64:encode/1 must return a binary");
    assert_eq!(
        bytes,
        &b"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7"[..],
        "base64:encode/1 must byte-equal the oracle (80 bytes, an off-heap refc binary)"
    );

    // A short input still round-trips through the same builder but stays inside the
    // on-heap limit: oracle `base64:encode(<<"A">>)` = `<<"QQ==">>`.
    let short = terms::make_heap_binary(&mut host, b"A").expect("a 1-byte binary");
    let got = run(&mut host, "base64", "encode", &[short]);
    assert_eq!(
        terms::binary_bytes(got).expect("a binary"),
        &b"QQ=="[..],
        "base64:encode/1 on a 1-byte input"
    );
}

// ---------------------------------------------------------------------------
// 4. Blocked mechanisms (asserted loudly, never silently passed)
// ---------------------------------------------------------------------------

/// Real functions that a **documented, still-open** gap blocks.
///
/// The property under test is: *the Rust runtime returns a typed [`BeamError`]
/// where the oracle returns a correct value* — never a silently-wrong value. Each
/// probe is printed with its exact typed error and that error is asserted, so a
/// later regression that turns a typed error into a *wrong result* is caught.
///
/// The `maps` module itself now **loads**: the `EXPORT_EXT`/`NEW_FUN_EXT`
/// fun-literal decode gap that used to block it (and, transitively, every
/// map-using stdlib module) is closed. What is still blocked is the harness's BIF
/// floor: the map-operation BIFs (`maps:put/3`, `maps:size/1`, `maps:get/2`, …)
/// are `erlang` BIFs the harness does not serve, so any call that needs them is a
/// typed `BadArg`.
#[test]
fn blocked_mechanisms_report_their_typed_error() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // ---- maps (loads now; its operations are still blocked) ---------------
    // `maps` must load: its literal area holds an external fun that
    // `beam_codec` now decodes (it used to fail with `unsupported_external_term`).
    host.load("maps")
        .expect("`maps` must now load (fun-literal decode)");

    // `maps:from_list/1` is pure Erlang that folds with `maps:put/3` — a BIF the
    // host floor does not serve — so the call is a typed `BadArg`, never a map.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[maps:from_list([{k,1}])]),halt().'
    //         #{k => 1}
    let k = host.intern("k");
    let pair = term_tuple(&mut host, &[k, make_small(1)]);
    let pairs = support::term_list(&mut host, &[pair]);
    let err = host
        .call("maps", "from_list", &[pairs])
        .expect_err("`maps:from_list/1` needs the unserved `maps:put/3` BIF");
    println!("SKIP: maps:from_list/1 — {err:?}");
    assert!(
        matches!(err, BeamError::BadArg),
        "unexpected `maps:from_list/1` error: {err:?}"
    );

    // ---- binaries / bitstrings -------------------------------------------
    // `base64:encode/1` — the canonical `bs_init_writable` +
    // `bs_create_bin`/`private_append` builder — is no longer blocked; it is
    // exercised positively by `sweep_base64_builds_an_off_heap_binary` above.
    //
    // `unicode:characters_to_binary/1` is a `unicode` BIF the host floor does not
    // serve, so the real module's call fails as a typed `BadArg`.
    //
    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_binary("hi")]),halt().'
    //         <<"hi">>
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

// ---------------------------------------------------------------------------
// 5. Execution evidence for modules the fun-literal decode unblocked
// ---------------------------------------------------------------------------

/// Modules that could not even **load** until `beam_codec` decoded the fun-literal
/// tags (`EXPORT_EXT`/`NEW_FUN_EXT`) now load and their pure compiled functions
/// *run*.
///
/// Both `maps` and `erl_scan` carry external-fun literals in their literal areas,
/// so before the decode landed they failed to load at all; now they load and
/// execute. (Most *map operations* are still host-BIF-floor-limited — see
/// [`blocked_mechanisms_report_their_typed_error`] — which is why `maps:new/0`,
/// i.e. `new() -> #{}`, is the exported `maps` function that is pure compiled code.)
#[test]
fn sweep_newly_loading_modules_execute() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = exec_host();

    // `maps` must now load; it could not before the fun-literal decode.
    host.load("maps").expect("`maps` must now load");

    // oracle: erl -noshell -eval 'io:format("~p~n",[maps:new()]),halt().'
    //         #{}
    let map = run(&mut host, "maps", "new", &[]);
    let entries = terms::map_entries(map).expect("maps:new/0 must return a map");
    assert!(
        entries.is_empty(),
        "maps:new/0 must be the empty map: {entries:?}"
    );
    let empty = terms::make_flatmap(&mut host, &[]).expect("build the oracle `#{}`");
    assert_term(&host, map, empty);

    // `erl_scan` likewise only loads because the fun-literal decode landed. Its
    // exported `reserved_word/1` is pure Erlang
    // (`case f_reserved_word(W) of true -> true; false -> lists:member(W,
    // erl_features:keywords()) end`); the *reserved-word* branch is a ~40-clause
    // compiled match that runs end to end here. (The non-reserved branch calls
    // `erl_features:keywords/0` — a **preloaded** module the sweep host cannot
    // load — so only reserved words are asserted.)
    //
    // oracle: erl -noshell -eval "io:format(\"~p~n\",[erl_scan:reserved_word('if')]),halt()."
    //         true
    let if_atom = host.intern("if");
    let got = run(&mut host, "erl_scan", "reserved_word", &[if_atom]);
    assert_atom(&host, got, "true");

    // oracle: erl -noshell -eval "io:format(\"~p~n\",[erl_scan:reserved_word('end')]),halt()."
    //         true
    let end_atom = host.intern("end");
    let got = run(&mut host, "erl_scan", "reserved_word", &[end_atom]);
    assert_atom(&host, got, "true");
}

// ---------------------------------------------------------------------------
// 6. Capability-gap inventory — every remaining gap pinned to its EXACT error
// ---------------------------------------------------------------------------
//
// Each gap below is driven through the **real** public execution path
// (`Machine::run_label` / `Machine::resume_outcome` over a synthetic `Module` +
// `MinimalContext`), exactly like `tests/opcodes.rs` and `tests/census.rs` do.
// The assertion is always the **exact** typed error (`unsupported_opcode: <what>`
// reason atom, or `BeamError::BadArg`) — never a wrong value, never the
// `families::UNCLAIMED_PREFIX` sentinel. A regression that turns a typed error
// into a value (or into the sentinel) fails here.
//
// Corpus reachability (evidence: `tests/census.rs`'s counts over the installed
// OTP's 261 stdlib/kernel/compiler modules, 793,319 instructions, plus
// `erl_features:configurable()` ⇒ `[compr_assign,maybe_expr]`):
//   * CORPUS-REACHABLE  — the compiler emits the instruction today; the gap is a
//     live limitation a real module can hit.
//   * FORWARD-COMPAT    — the instruction is never emitted by the installed
//     toolchain (`native_record` is not a configurable feature, `erlc
//     -enable-feature native_record` ⇒ "the feature 'native_record' does not
//     exist", and a `-record` module disassembles to only `update_record`), so
//     the gap exists only for a future toolchain that emits it.

/// Map every `Instr::Label` in `code` to its index (the loader's contract).
fn gap_labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for the synthetic modules the gap inventory drives.
struct GapFixture {
    module: Module,
}

impl GapFixture {
    fn new() -> GapFixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("gap");
        GapFixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the first label in `code`).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = gap_labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Drive one function of `module` through the real public entry with the default
/// (`MinimalContext`) context: no BIF hook, no apply hook, **no mailbox**.
fn run_gap(module: &Module, entry: Label, args: &[Eterm]) -> std::result::Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.run_label(module, entry, args.len() as u32, args, &mut ctx)
}

/// Drive one function with a caller-supplied context (for a context that is *not*
/// `MinimalContext`).
fn run_gap_with(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> std::result::Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_label(module, entry, args.len() as u32, args, &mut ctx)
}

/// Pin `result` to the exact typed `unsupported_opcode: <want>` error, print the
/// one-line `GAP:` inventory entry, and prove it is **not** the unclaimed sentinel.
fn pin(name: &str, result: std::result::Result<Eterm, BeamError>, want: &str) {
    let want_atom = format!("unsupported_opcode: {want}");
    match result {
        Err(BeamError::Raise(raise)) => {
            let got = raise.reason.as_atom().unwrap_or("<non-atom reason>");
            println!("GAP: {name} — {got}");
            assert_eq!(got, want_atom, "gap `{name}` produced the wrong typed error");
            assert!(
                !want_atom.contains("widened opcode not yet executed"),
                "gap `{name}` must never be the unclaimed sentinel"
            );
        }
        other => panic!("GAP `{name}`: expected the typed error {want_atom:?}, got {other:?}"),
    }
}

/// Pin `result` to the typed `badarg` (`BeamError::BadArg` — `bad_register` /
/// `unknown_label` carry no atom) and print the inventory entry.
fn pin_badarg(name: &str, result: std::result::Result<Eterm, BeamError>) {
    println!("GAP: {name} — BeamError::BadArg (the BEAM `badarg` atom)");
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("GAP `{name}`: expected BeamError::BadArg, got {other:?}"),
    }
}

/// The whole inventory. Groups mirror the owning family/file; each line names the
/// exact typed error the engine produces today.
#[test]
fn interpreter_capability_gaps_report_their_exact_typed_error() {
    // --- core loop: the `NO_FAIL` guard branches (src/interpreter.rs) ---------
    // CORPUS-REACHABLE: `test`/`select_val` are emitted for every match.
    {
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            // `is_tuple` on a small int fails, and `fail` is `NO_FAIL` (the
            // compiler's "cannot fail" marker) ⇒ typed error, never a fall-through.
            Instr::Test {
                kind: TestKind::IsTuple,
                fail: NO_FAIL,
                args: vec![Operand::X(0)],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "test/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "test/no_fail",
        );
    }
    {
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(0),
                fail: NO_FAIL,
                pairs: vec![(Operand::Integer(99), Label(2))],
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "select_val/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "select_val/no_fail",
        );
    }

    // --- maps family (src/families/maps.rs) ----------------------------------
    // CORPUS-REACHABLE: map reads/updates are emitted by every map-using module.
    {
        // `get_map_elements` with an absent key and `fail == NO_FAIL`. The map is
        // built in a separate context that stays alive for the run.
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let b = f.atom("b");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::GetMapElements {
                fail: NO_FAIL,
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1))],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();

        let mut builder = MinimalContext::new();
        let map = terms::make_flatmap(&mut builder, &[(beam_term::make_atom(b), make_small(1))])
            .expect("a flat map");
        pin(
            "get_map_elements/no_fail",
            run_gap(&module, entry, &[map]),
            "get_map_elements: key absent with no fail label",
        );
    }
    {
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let b = f.atom("b");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::HasMapFields {
                fail: NO_FAIL,
                source: Operand::X(0),
                keys: vec![Operand::Atom(a)],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();

        let mut builder = MinimalContext::new();
        let map = terms::make_flatmap(&mut builder, &[(beam_term::make_atom(b), make_small(1))])
            .expect("a flat map");
        pin(
            "has_map_fields/no_fail",
            run_gap(&module, entry, &[map]),
            "has_map_fields: key absent with no fail label",
        );
    }
    {
        // `put_map_assoc` / `put_map_exact` with a **non-map** source and
        // `fail == NO_FAIL`: the C ground truth is a `{badmap, Source}` raise, and
        // the engine can only raise it if the module can name the `badmap` atom.
        // This module deliberately cannot, so the gap surfaces as its exact
        // "the tag atom is absent" error rather than a fabricated atom.
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let assoc = vec![
            Instr::Label(Label(1)),
            Instr::PutMapAssoc {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, assoc);
        let module = f.finish();
        pin(
            "put_map_assoc/no_fail",
            run_gap(&module, entry, &[make_small(7)]),
            "maps: no 'badmap' atom in the module",
        );
    }
    {
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let exact = vec![
            Instr::Label(Label(1)),
            Instr::PutMapExact {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, exact);
        let module = f.finish();
        pin(
            "put_map_exact/no_fail",
            run_gap(&module, entry, &[make_small(7)]),
            "maps: no 'badmap' atom in the module",
        );
    }

    // --- records family (src/families/records/mod.rs) ------------------------
    // FORWARD-COMPAT: the installed toolchain never emits the `native_record`
    // family (`erl_features:configurable()` ⇒ `[compr_assign,maybe_expr]`; `erlc
    // -enable-feature native_record` ⇒ "the feature 'native_record' does not
    // exist"), so all 7 opcodes have ZERO corpus occurrences. Pinned anyway so a
    // future toolchain cannot regress them silently.
    {
        // `is_any_native_record` with a non-record source and `NO_FAIL`.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::IsAnyNativeRecord {
                fail: NO_FAIL,
                src: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "is_any_native_record/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "is_any_native_record/no_fail",
        );
    }
    {
        // `put_record`'s **create** form (`Src == nil`): the C resolves the record
        // definition through the code server's record table (`erl_record.c:102-170`),
        // a facility this runtime does not have. FORWARD-COMPAT (see above).
        let mut f = GapFixture::new();
        let rec = f.atom("rec");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::PutRecord {
                fail: NO_FAIL,
                id: Operand::Atom(rec),
                src: Operand::Nil,
                dst: Operand::X(0),
                live: 0,
                updates: vec![],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "put_record/create",
            run_gap(&module, entry, &[]),
            "put_record: create (record definition table unavailable)",
        );
    }
    {
        // `put_record`'s **update** form with a non-record source and `NO_FAIL`.
        let mut f = GapFixture::new();
        let rec = f.atom("rec");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::PutRecord {
                fail: NO_FAIL,
                id: Operand::Atom(rec),
                src: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                updates: vec![],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "put_record/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "put_record/no_fail",
        );
    }

    // --- bitstrings family (src/families/bitstrings/{create,raw}.rs) ---------
    // CORPUS-REACHABLE: `bs_create_bin` is emitted for every binary builder.
    {
        // An `append`/`private_append` segment that is **not** the first: the
        // compiler only ever emits it first; C seeds the build from the source, so
        // a non-first one is refused rather than mis-handled. The segment list is
        // `[Type, Index, Unit, Flags, Value, Size]` per segment.
        let mut f = GapFixture::new();
        let integer = f.atom("integer");
        let append = f.atom("append");
        let segments = vec![
            // segment 0 — a valid `integer:8` segment.
            Operand::Atom(integer),
            Operand::Integer(1),
            Operand::Integer(1),
            Operand::Nil,
            Operand::Integer(7),
            Operand::Integer(8),
            // segment 6 — the misplaced `append`.
            Operand::Atom(append),
            Operand::Integer(0),
            Operand::Integer(1),
            Operand::Nil,
            Operand::Integer(0),
            Operand::Integer(0),
        ];
        let code = vec![
            Instr::Label(Label(1)),
            Instr::BsCreateBin {
                fail: NO_FAIL,
                alloc: 0,
                live: 1,
                unit: 8,
                dst: Operand::X(1),
                segments,
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "bs_create_bin/append-not-first",
            run_gap(&module, entry, &[]),
            "bs_create_bin: append/private_append is not the first segment",
        );
    }
    {
        // A refc-binary `ErlSubBits` window that does **not** start on a byte
        // boundary: `raw::heap_bits` refuses it (the engine only ever builds
        // byte-aligned refc binaries), so a match-context op over it reports the
        // unreadable subject instead of reading the wrong bytes.
        //
        // `bs_get_tail` takes its context operand and reads `orig` out of it; the
        // misaligned refc term *is* an `ErlSubBits`, whose `orig` is the bare
        // `BinRef` — which `heap_bits` refuses.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::BsGetTail {
                ctx: Operand::X(0),
                dst: Operand::X(1),
                live: 1,
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();

        // Keep the misaligned refc term (and its payload) alive for the run. Both
        // are word-aligned (a `Vec<Eterm>`), satisfying `beam_term::refc`'s
        // frozen-layout contract; `offset_bits = 1` is the misalignment under test.
        let mut builder = MinimalContext::new();
        let mut payload: Vec<Eterm> = vec![Eterm::default(); 2];
        let base = payload.as_ptr().cast::<u8>();
        let value = payload.as_mut_ptr();
        let hp = builder
            .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
            .expect("the arena hands out 8 words");
        // SAFETY: `hp` points at `ERL_REFC_BITS_SIZE` (8) exclusively-owned words in
        // the context's pointer-stable arena, `value`/`base` point at the live,
        // word-aligned `payload`, and `next` is null (a chain terminator). The
        // returned term is used only while `builder`/`payload` live.
        let refc = unsafe {
            beam_term::refc::write_refc_binary(
                hp,
                value,
                core::ptr::null_mut(),
                base,
                1,
                16,
            )
        };
        // The term must survive the run: pass it as the argument (the payload stays
        // alive in `payload`/`builder` for the whole call).
        pin(
            "bitstring/refc-window-not-byte-aligned",
            run_gap(&module, entry, &[refc]),
            "bitstring: subject is not a readable binary",
        );
        core::hint::black_box(&payload);
    }

    // --- exceptions family (src/families/exceptions.rs) ----------------------
    // CORPUS-REACHABLE: `badmatch`/`raise`/`try_case` are emitted for every match.
    {
        // A `badmatch` raise on a module whose atom table cannot name the tag: the
        // engine refuses to substitute a *different* atom.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Badmatch {
                value: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "exceptions/unnameable-raise-reason-atom",
            run_gap(&module, entry, &[make_small(0)]),
            "exceptions: no 'badmatch' atom in the module",
        );
    }
    {
        // `raise` with no exception currently being handled (`pending_class` unset).
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Raise {
                trace: Operand::Nil,
                value: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "exceptions/raise-with-nothing-handled",
            run_gap(&module, entry, &[make_small(0)]),
            "exceptions: raise with no exception being handled",
        );
    }
    {
        // `try_case` reached without an exception having landed: `x(0)` is not the
        // `THE_NON_VALUE` marker. `Try` pushes the (balanced) catch frame first.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Try {
                frame: Operand::X(1),
                fail: Label(2),
            },
            Instr::TryCase {
                frame: Operand::X(2),
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "try_case/x0-not-THE_NON_VALUE",
            run_gap(&module, entry, &[]),
            "try_case: x(0) is not THE_NON_VALUE (no exception landed)",
        );
    }
    {
        // An unbalanced catch-frame pop: `try_case` with no frame pushed.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::TryCase {
                frame: Operand::X(1),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "try_case/unbalanced-catch-frames",
            run_gap(&module, entry, &[]),
            "try_case: unbalanced catch frames",
        );
    }

    // --- `ExecutionContext` trait defaults (src/context.rs) ------------------
    // CORPUS-REACHABLE: a `MinimalContext` with no hook installed.
    {
        // `apply_fun` with no apply hook (`call_fun2`).
        let mut f = GapFixture::new();
        let safe = f.atom("safe");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::CallFun2 {
                tag: Operand::Atom(safe),
                arity: 0,
                func: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "context/apply_fun-has-no-hook",
            run_gap(&module, entry, &[]),
            "apply_fun",
        );
    }
    {
        // The nine mailbox seam methods, each the trait's typed `unsupported` on a
        // bare `MinimalContext` (no mailbox installed).
        let cases: Vec<(&str, Instr, &str)> = vec![
            ("send", Instr::Send, "send_message"),
            (
                "loop_rec",
                Instr::LoopRec {
                    fail: Label(2),
                    dst: Operand::X(1),
                },
                "receive_next",
            ),
            (
                "loop_rec_end",
                Instr::LoopRecEnd { target: Label(2) },
                "receive_skip",
            ),
            ("timeout", Instr::Timeout, "receive_reset"),
            ("remove_message", Instr::RemoveMessage, "receive_remove"),
            (
                "recv_marker_reserve",
                Instr::RecvMarkerReserve {
                    marker: Operand::X(1),
                },
                "receive_marker_reserve",
            ),
            (
                "recv_marker_bind",
                Instr::RecvMarkerBind {
                    marker: Operand::X(1),
                    reference: Operand::X(2),
                },
                "receive_marker_bind",
            ),
            (
                "recv_marker_use",
                Instr::RecvMarkerUse {
                    reference: Operand::X(1),
                },
                "receive_marker_use",
            ),
            (
                "recv_marker_clear",
                Instr::RecvMarkerClear {
                    reference: Operand::X(1),
                },
                "receive_marker_clear",
            ),
        ];
        for (label, instr, method) in cases {
            let mut f = GapFixture::new();
            let code = vec![Instr::Label(Label(1)), instr, Instr::Label(Label(2)), Instr::Return];
            let entry = f.add_function("f", 0, code);
            let module = f.finish();
            pin(
                &format!("context/mailbox-{label}"),
                run_gap(&module, entry, &[]),
                method,
            );
        }
    }
    {
        // `receive` with no mailbox: `wait`/`wait_timeout(non-zero)` **suspend**, and
        // the non-blocking entry folds that into the documented typed error (there
        // is no host timer at M1, so a non-zero/`infinity` timeout suspends too).
        let mut f = GapFixture::new();
        let infinity = f.atom("infinity");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::WaitTimeout {
                target: Label(2),
                time: Operand::Atom(infinity),
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "receive/wait_timeout-infinity-suspends",
            run_gap(&module, entry, &[]),
            "run: process blocked (no message available)",
        );
    }
    {
        // `alloc_words` on a context that does **not** override it (the trait
        // default): a core `move` of an out-of-range integer materializes an exact
        // bignum, which needs raw heap words.
        struct BareCtx;
        impl ExecutionContext for BareCtx {
            fn call_bif(
                &mut self,
                _module: &str,
                _function: &str,
                _arity: u32,
                _args: &[Eterm],
            ) -> std::result::Result<Eterm, BeamError> {
                Err(BeamError::BadArg)
            }
            fn ensure_heap(
                &mut self,
                _heap_need: u32,
                _live: u32,
                _x_regs: &mut [Eterm],
                _stack: &mut [Eterm],
            ) -> std::result::Result<(), BeamError> {
                Ok(())
            }
        }

        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Integer(beam_term::MAX_SMALL + 1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        let mut ctx = BareCtx;
        pin(
            "context/alloc_words-trait-default",
            run_gap_with(&module, entry, &[], &mut ctx),
            "alloc_words",
        );
    }

    // --- interpreter / operand guards (src/interpreter.rs, src/operand.rs) ----
    // CORPUS-REACHABLE: malformed/edge bytecode paths.
    {
        let mut f = GapFixture::new();
        let entry = f.add_function("f", 0, vec![Instr::Label(Label(1)), Instr::Return]);
        let module = f.finish();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        // A `pc` past the function's last instruction is the loop's own typed error.
        pin(
            "run/pc-out-of-range",
            machine
                .resume_outcome(&module, 0, 99_999, &mut ctx)
                .map(|_| make_small(0)),
            "run: pc out of range",
        );
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        pin(
            "run/function-index-out-of-range",
            machine
                .resume_outcome(&module, 99, 0, &mut ctx)
                .map(|_| make_small(0)),
            "run: function index out of range",
        );
    }
    {
        // An out-of-range register (`bad_register`) and an absent jump target
        // (`unknown_label`) are both the typed BEAM `badarg`.
        let mut f = GapFixture::new();
        let entry = f.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::X(2000),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        let module = f.finish();
        pin_badarg("operand/bad_register", run_gap(&module, entry, &[]));

        let mut f = GapFixture::new();
        let entry = f.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Jump { target: Label(99) },
                Instr::Return,
            ],
        );
        let module = f.finish();
        pin_badarg("jump/unknown_label", run_gap(&module, entry, &[]));
    }
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

