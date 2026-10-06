//! **Differential ETS evidence suite**: one battery of ETS operations, run twice
//! — against the *system* `erl` (the OTP 29 / erts-17.1 oracle) and against this
//! crate's `beam_ets` — and compared as identical canonical renderings.
//!
//! # Why this file exists
//!
//! The crate's own unit tests assert *our* understanding of `ets:*`. This suite
//! asserts the thing that actually matters for contract A.1 (term/key/value
//! semantics) and A.8 (`ets` introspection): that a real `erl` observably agrees.
//! Every expectation below was **derived from `erl`**, not written from memory.
//!
//! # How the comparison works
//!
//! * One battery of cases (`battery.rs` on the Rust side, the embedded Erlang
//!   program in `erl_program.rs` on the oracle side) performs the same operations
//!   in the same order.
//! * Every result is reduced to a canonical textual form — small integers, atoms,
//!   tuples and proper lists only — by the *same* ~20-line renderer on each side
//!   (`render`/`t`/`l`/`srt`/`inorder`/`cap`). Table ids/handles, `memory`, `pids`
//!   and floats never appear; anything whose order is not part of the contract is
//!   rendered **sorted** (`srt`), and only `ordered_set` iteration is rendered in
//!   order (`inorder`).
//! * Each case prints/returns exactly one `case_name -> rendered_result` pair, so
//!   the two runs are compared line by line. `term_to_binary`/ETF hex is *not*
//!   used: byte-identical encoding across the two implementations is not
//!   guaranteed, whereas the textual renderer is.
//!
//! # What is asserted
//!
//! 1. [`rust_battery_matches_the_embedded_oracle_table`] — our implementation
//!    matches the `erl`-derived table. Runs **always**, so the expectations stay
//!    meaningful even on a machine with no Erlang.
//! 2. [`oracle_battery_matches_the_embedded_oracle_table`] — the live oracle
//!    still reproduces the embedded table (catches an oracle upgrade or a stale
//!    expectation).
//! 3. [`rust_battery_agrees_with_the_live_oracle`] — the real differential gate:
//!    live `erl` output vs live `beam_ets` output, case by case.
//!
//! # Skipping without Erlang
//!
//! Tests 2 and 3 resolve `erl`/`erlc` from `$ERTS_ORACLE` and then `$PATH`. When
//! they are absent the test prints a note and returns — CI without Erlang stays
//! green. Set `BEAM_ETS_REQUIRE_ORACLE=1` to turn that skip into a failure (what
//! this repository's gate run does, so a silently-skipped differential can never
//! look like a pass).
//!
//! # Battery coverage
//!
//! All four kinds (`set`, `ordered_set`, `bag`, `duplicate_bag`) × insert /
//! insert_new / duplicate-object / delete_object (matching + non-matching) /
//! tab2list+size / delete-by-key / take; `{keypos, 2}` insert+lookup+update;
//! `ordered_set` navigation (`first`/`next`/`last`/`prev`, absent anchors, empty
//! table, `'$end_of_table'`); `update_counter/3,4` (scalar, `{Pos,Incr}`,
//! `{Pos,Incr,Threshold,SetValue}`, operation lists, defaults, absent keys,
//! counter-incapable kinds); `update_element/3`; `lookup_element/3,4`; `select/2`
//! with variables/wildcards/literals/guards (comparison, type test,
//! `andalso`/`orelse`) and bodies (`'$_'`, `'$N'`, constants, `{{…}}`
//! construction); `select_delete/2`+`select_count/2`; the `match` family;
//! malformed specs (`badarg`); and two 10 001-key tables (`set` + `ordered_set`)
//! exercising size, lookups, the ordered `next` chain and a guarded
//! `select_count`.
//!
//! # Behaviours real ETS forced (recorded here and in the case comments)
//!
//! * **`ets:size/1` does not exist in OTP 29** — `ets:info(T, size)` is the
//!   supported spelling (`erlang:function_exported(ets, size, 1)` is `false`; a
//!   call raises `undef`). The oracle program therefore uses `ets:info/2`.
//! * **A match-spec body is ANDed into one result**: only the *last* body element
//!   survives (`[{'$1','$2'},[],['$1','$2']]` returns `[2]`), which is why every
//!   case here uses a single-element body.
//! * **`{{…}}` is the tuple-construction form** — a one-element tuple whose single
//!   element is a tuple template (see `select_body_construct`).
//! * **`ets:update_element/3` accepts `pos == arity`** (and rejects `pos == keypos`
//!   and `pos == 0`) — `update_element_variants` pins the `badarg` boundary at
//!   `pos > arity`.
//! * **`ets:update_counter/4` accepts an empty operation list `[]`** (it deposits
//!   the default object and returns `[]`), while `ets:update_element(T, K, [])` is
//!   `true` and changes nothing.
//! * **`ets:match_delete/2` answers `true`**, not the number deleted — our
//!   `Table::match_delete` returns the count, so the differential renders a
//!   successful call as `true` and verifies the deletion through `size` and
//!   `tab2list` inside the same case.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// The support modules live in `tests/differential/` so that they are *not*
// themselves integration-test targets (Cargo only builds the `.rs` files
// directly under `tests/` as targets); the explicit `#[path]` is needed because
// a crate root resolves its modules relative to its own directory.
#[path = "differential/battery/mod.rs"]
mod battery;
#[path = "differential/erl_program.rs"]
mod erl_program;

use battery::rust_battery;

/// How long the oracle is given to compile and then to run. Generous: the two
/// 10 001-key cases build and walk a large table, and a loaded CI box can be slow.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The `erl`-derived reference rendering of every case.
///
/// Derived by running the program in `erl_program.rs` under the system `erl`
/// (OTP 29 / erts-17.1) — never written by hand. Kept in the oracle's `emit/2`
/// order (the map below sorts, but the *list* order is the readable record).
const EXPECTED: &[(&str, &str)] = &[
    ("set_insert_lookup", "{[{1,10}],[],true,false,3,2,false}"),
    ("set_insert_new", "{true,false,true,2,[{1,10}]}"),
    ("set_identical_object", "{1,[{1,20}]}"),
    ("set_delete_object_same", "{0,false,[]}"),
    ("set_delete_object_other", "{1,true,[{1,20}]}"),
    ("set_tab2list_count", "{5,5}"),
    ("set_delete_by_key", "{1,false,true}"),
    ("set_take", "{[{1,10}],[],1,false}"),
    (
        "ordered_set_insert_lookup",
        "{[{1,10}],[],true,false,3,2,false}",
    ),
    ("ordered_set_insert_new", "{true,false,true,2,[{1,10}]}"),
    ("ordered_set_identical_object", "{1,[{1,20}]}"),
    ("ordered_set_delete_object_same", "{0,false,[]}"),
    ("ordered_set_delete_object_other", "{1,true,[{1,20}]}"),
    ("ordered_set_tab2list_count", "{5,5}"),
    ("ordered_set_delete_by_key", "{1,false,true}"),
    ("ordered_set_take", "{[{1,10}],[],1,false}"),
    ("bag_insert_lookup", "{[{1,10}],[],true,false,3,2,false}"),
    ("bag_insert_new", "{true,false,true,2,[{1,10}]}"),
    ("bag_identical_object", "{2,[{1,10},{1,20}]}"),
    ("bag_delete_object_same", "{1,true,[{1,10}]}"),
    ("bag_delete_object_other", "{2,true,[{1,10},{1,20}]}"),
    ("bag_tab2list_count", "{5,5}"),
    ("bag_delete_by_key", "{1,false,true}"),
    ("bag_take", "{[{1,10},{1,20}],[],1,false}"),
    (
        "duplicate_bag_insert_lookup",
        "{[{1,10}],[],true,false,3,2,false}",
    ),
    ("duplicate_bag_insert_new", "{true,false,true,2,[{1,10}]}"),
    (
        "duplicate_bag_identical_object",
        "{3,[{1,10},{1,10},{1,20}]}",
    ),
    ("duplicate_bag_delete_object_same", "{1,true,[{1,10}]}"),
    (
        "duplicate_bag_delete_object_other",
        "{2,true,[{1,10},{1,20}]}",
    ),
    ("duplicate_bag_tab2list_count", "{5,5}"),
    ("duplicate_bag_delete_by_key", "{1,false,true}"),
    ("duplicate_bag_take", "{[{1,10},{1,20}],[],1,false}"),
    ("keypos2_basic", "{[{a,1}],[{z,1}],2,true,true}"),
    ("keypos2_update_counter", "{15,[{tag,1,15}]}"),
    ("keypos2_update_element", "{true,[{tag,1,99,20}]}"),
    (
        "ordered_iteration",
        "{1,3,[1,2,3],[3,2,1],[1,2,3],$end_of_table,$end_of_table}",
    ),
    (
        "ordered_absent_bounds",
        "{20,10,10,$end_of_table,$end_of_table,30}",
    ),
    (
        "ordered_empty_nav",
        "{$end_of_table,$end_of_table,$end_of_table,$end_of_table}",
    ),
    ("update_counter_3_variants", "{15,12,25,[13,26],[{1,13,26}]}"),
    ("update_counter_3_absent", "{badarg,badarg,0}"),
    ("update_counter_3_threshold", "[ok:100,ok:100]"),
    (
        "update_counter_4_default",
        "{5,[{1,5,0}],10,3,[{7,3,0}]}",
    ),
    (
        "update_counter_4_list",
        "{[11,21],[{1,11,21}],[16],[],[{9,0,0}]}",
    ),
    (
        "update_counter_4_threshold",
        "[{100,[{1,100}]},{12,[{1,12}]},{100,[{1,100}]},{3,[{1,3}]},{100,[{1,100}]}]",
    ),
    ("update_counter_multi_kind", "[badarg,badarg]"),
    (
        "update_counter_bignum_overflow",
        "{576460752303423488,[{1,576460752303423488}],576460752303423487,[{1,576460752303423487}]}",
    ),
    (
        "update_counter_bignum_kinds",
        "[{576460752303423488,1267650600228805862249006628864,[{1,1267650600228805862249006628864}]},{576460752303423488,1267650600228805862249006628864,[{1,1267650600228805862249006628864}]}]",
    ),
    (
        "update_counter_bignum_operand",
        "{1267650600228229401496703205386,10,[{1,10}]}",
    ),
    (
        "update_counter_bignum_element",
        "{-1267650600228229401496703205371,-1267650600228229401496703205374,[{1,-1267650600228229401496703205374}]}",
    ),
    (
        "update_counter_bignum_add",
        "{1613215145994376956305797881764370268938305349227256869814279,6277101735386680763835789423207666416102355444464034512903,[{1,6277101735386680763835789423207666416102355444464034512903}]}",
    ),
    (
        "update_counter_bignum_mixed_sign",
        "{0,-1267650600228229401496703205377,-1,[{1,-1}]}",
    ),
    (
        "update_counter_bignum_threshold",
        "[{7,[{1,7}]},{-7,[{1,-7}]},{1267650600228229401496703205376,[{1,1267650600228229401496703205376}]}]",
    ),
    (
        "update_counter_bignum_warp_value",
        "{1267650600228229401496703205376,[{1,1267650600228229401496703205376}]}",
    ),
    (
        "update_counter_bignum_list_ops",
        "{[1267650600228229401496703205376,-1267650600228229401496703205376],[{1,1267650600228229401496703205376,-1267650600228229401496703205376}]}",
    ),
    (
        "update_counter_bignum_position_badarg",
        "{badarg,ok:11,badarg,badarg,[{1,11}]}",
    ),
    (
        "update_element_variants",
        "{true,[{1,99,20,30}],true,[{1,5,2,30}],true,[{1,5,2,30}],false,badarg,badarg,badarg,[{1,5,2,30}]}",
    ),
    (
        "take_and_lookup_element",
        "{10,20,badarg,badarg,badarg,default,10,default,badarg}",
    ),
    ("select_head_vars", "[10,20]"),
    ("select_wildcard_literal", "[{1,a,10}]"),
    ("select_guard_comparison", "[2,3]"),
    ("select_guard_typetest", "[2]"),
    ("select_guard_andalso_orelse", "{[1,2],[1,3]}"),
    ("select_body_whole", "[{1,a,10},{2,b,20}]"),
    ("select_body_var", "[1,2]"),
    ("select_body_const", "[ok,ok]"),
    ("select_body_construct", "[{1,a},{2,b}]"),
    ("select_delete_and_count", "{0,0,2,2,1,[1]}"),
    (
        "match_family",
        "{[{1,a,10},{2,a,20},{3,a,30}],[[1,10],[2,20],[3,30]],true,3,2,[2,3]}",
    ),
    (
        "malformed_specs",
        "[badarg,badarg,badarg,badarg,badarg,badarg,badarg,ok:[{1,10}]]",
    ),
    (
        "set_large",
        "{10001,[{0,0},{10000,20000},{5000,10000}],5000}",
    ),
    (
        "ordered_large",
        "{10001,0,10000,10001,5001,$end_of_table,[{0,0},{10000,20000},{5000,10000}],5000}",
    ),
];

// ---------------------------------------------------------------------------
// The oracle: resolving `erl`/`erlc`, running the battery
// ---------------------------------------------------------------------------

/// The resolved oracle executables.
struct Oracle {
    erl: PathBuf,
    erlc: PathBuf,
}

/// Whether `$BEAM_ETS_REQUIRE_ORACLE` asks for the oracle to be mandatory.
fn oracle_required() -> bool {
    match env::var("BEAM_ETS_REQUIRE_ORACLE") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Resolve the oracle from `$ERTS_ORACLE`, then `$PATH`.
///
/// Returns `None` when no Erlang is installed (the caller skips), but **fails
/// loudly** when `$ERTS_ORACLE` names something that is not an executable: a
/// misconfigured override must not be mistaken for "no oracle on this machine".
fn resolve_oracle() -> Option<Oracle> {
    let erl = match env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let candidate = PathBuf::from(&value);
            assert!(
                is_executable(&candidate),
                "$ERTS_ORACLE is set to '{}', which is not an executable file",
                candidate.display()
            );
            candidate
        }
        None => find_in_path("erl")?,
    };
    // `erlc` normally sits next to `erl`; fall back to `$PATH` (a wrapper script
    // may install only `erl`).
    let sibling = erl.with_file_name("erlc");
    let erlc = if is_executable(&sibling) {
        sibling
    } else {
        find_in_path("erlc")?
    };
    Some(Oracle { erl, erlc })
}

/// The first executable `name` on `$PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Whether `path` is an executable file.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Compile the embedded oracle program and run its battery, returning the
/// `case_name -> rendered_result` pairs it emitted.
///
/// Panics with the captured stderr when the oracle fails to compile, crashes its
/// battery (`BATTERY_CRASH`), exits non-zero or exceeds [`ORACLE_TIMEOUT`] — a
/// broken oracle must never be mistaken for a skip.
fn run_oracle(oracle: &Oracle) -> Vec<(String, String)> {
    // The tests run concurrently in one process, so the scratch directory must be
    // unique per *call*, not just per process.
    static ATTEMPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "beam_ets_diff_oracle_{}_{attempt}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the oracle scratch directory is creatable");
    let source = dir.join("beam_ets_diff.erl");
    fs::write(&source, erl_program::ERL_PROGRAM).expect("the oracle program is writable");

    let mut compile = Command::new(&oracle.erlc);
    compile
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .current_dir(&dir);
    let compiled = run_bounded(&mut compile, ORACLE_TIMEOUT).unwrap_or_else(|error| {
        panic!(
            "erlc ({}) could not compile the oracle program: {error}",
            oracle.erlc.display()
        )
    });
    assert!(
        compiled.status.success(),
        "erlc ({}) failed with {}:\n{}",
        oracle.erlc.display(),
        compiled.status,
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut run = Command::new(&oracle.erl);
    run.arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        // `current_dir` confines a crash dump (`erl_crash.dump`) to the scratch
        // directory instead of polluting the repository.
        .current_dir(&dir)
        .arg("-eval")
        .arg("beam_ets_diff:run()")
        .arg("-s")
        .arg("init")
        .arg("stop");
    let output = run_bounded(&mut run, ORACLE_TIMEOUT).unwrap_or_else(|error| {
        panic!(
            "erl ({}) could not run the oracle battery: {error}",
            oracle.erl.display()
        )
    });
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !stderr.contains("BATTERY_CRASH"),
        "the oracle battery crashed:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "erl ({}) exited with {}:\n{stderr}",
        oracle.erl.display(),
        output.status
    );

    let emitted = parse_emitted(&String::from_utf8_lossy(&output.stdout));
    let _ = fs::remove_dir_all(&dir);
    emitted
}

/// Spawn `command`, wait at most `timeout` for it, and collect its output.
///
/// Both pipes are read only after the child has exited; the battery writes a few
/// kilobytes, far below the pipe capacity, so that cannot deadlock.
fn run_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to spawn: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {timeout:?}"));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed while waiting: {error}"));
            }
        }
    }
    child
        .wait_with_output()
        .map_err(|error| format!("failed to collect the output: {error}"))
}

/// Parse the oracle's `name|rendered` lines.
fn parse_emitted(stdout: &str) -> Vec<(String, String)> {
    let mut cases = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (name, rendered) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("the oracle emitted a malformed line: {line:?}"));
        cases.push((name.to_string(), rendered.to_string()));
    }
    cases
}

// ---------------------------------------------------------------------------
// Comparing
// ---------------------------------------------------------------------------

/// Index cases by name, asserting the names are unique (a duplicate would
/// otherwise silently shadow a case).
fn case_map(cases: &[(String, String)], what: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (name, rendered) in cases {
        assert!(
            map.insert(name.clone(), rendered.clone()).is_none(),
            "{what} emitted the case `{name}` twice"
        );
    }
    map
}

/// The embedded table as a map (asserting it has no duplicate case names).
fn expected_map() -> BTreeMap<String, String> {
    let cases: Vec<(String, String)> = EXPECTED
        .iter()
        .map(|(name, rendered)| (name.to_string(), rendered.to_string()))
        .collect();
    case_map(&cases, "the embedded expectation table")
}

/// Cases where `beam_ets` is **known to differ from the system `erl` today**.
///
/// These are *recorded*, not tolerated: they are excluded from the equality
/// assertion and pinned by [`known_divergences_are_still_present`], which fails —
/// telling you to delete the entry — the moment `beam_ets` is fixed. Keeping them
/// in the battery (rather than dropping them) keeps the oracle's real behaviour in
/// `EXPECTED` and the gap actionable.
///
/// **Currently empty**: every case in the battery agrees with the live oracle, so
/// the suite runs with no exclusions.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[];

/// `true` when `name` is a recorded divergence.
fn is_known_divergence(name: &str) -> bool {
    KNOWN_DIVERGENCES.iter().any(|(case, _)| *case == name)
}

/// A human-readable report of every case the two sides disagree about, or `None`
/// when they agree completely. Recorded divergences are skipped.
fn differences(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> Option<String> {
    let mut report = String::new();
    for (name, wanted) in expected {
        if is_known_divergence(name) {
            continue;
        }
        match actual.get(name) {
            None => report.push_str(&format!("  {name}: MISSING (expected {wanted})\n")),
            Some(got) if got != wanted => {
                report.push_str(&format!("  {name}:\n    expected: {wanted}\n    actual:   {got}\n"));
            }
            Some(_) => {}
        }
    }
    for name in actual.keys() {
        if !expected.contains_key(name) && !is_known_divergence(name) {
            report.push_str(&format!("  {name}: UNEXPECTED\n"));
        }
    }
    if report.is_empty() {
        None
    } else {
        Some(report)
    }
}

/// Assert two case maps agree, with a full per-case report on failure.
fn assert_agree(expected: &BTreeMap<String, String>, actual: &BTreeMap<String, String>, what: &str) {
    if let Some(report) = differences(expected, actual) {
        panic!("{what} differs from the oracle reference:\n{report}");
    }
}

/// Resolve the oracle, or skip the calling test with a note.
///
/// `None` means "skip": no Erlang on this machine. When
/// `$BEAM_ETS_REQUIRE_ORACLE` is set, a missing oracle is a failure instead.
fn oracle_or_skip(test: &str) -> Option<Oracle> {
    match resolve_oracle() {
        Some(oracle) => Some(oracle),
        None => {
            assert!(
                !oracle_required(),
                "{test}: $BEAM_ETS_REQUIRE_ORACLE is set but neither $ERTS_ORACLE nor an \
                 executable `erl` on $PATH could be found"
            );
            eprintln!(
                "{test}: SKIPPED — no Erlang oracle found ($ERTS_ORACLE unset and no executable \
                 `erl` on $PATH). The embedded, erl-derived expectation table is still checked by \
                 `rust_battery_matches_the_embedded_oracle_table`."
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Our implementation against the `erl`-derived table. Always runs, so the
/// differential evidence keeps its meaning on a machine without Erlang.
#[test]
fn rust_battery_matches_the_embedded_oracle_table() {
    let expected = expected_map();
    let rust = case_map(&rust_battery(), "the beam_ets battery");
    assert_agree(&expected, &rust, "the beam_ets battery");
}

/// The live oracle against the embedded table: catches an oracle upgrade (a
/// changed ETS semantic) or a stale expectation.
#[test]
fn oracle_battery_matches_the_embedded_oracle_table() {
    let Some(oracle) = oracle_or_skip("oracle_battery_matches_the_embedded_oracle_table") else {
        return;
    };
    let emitted = run_oracle(&oracle);
    eprintln!(
        "[differential] oracle: {} — {} cases emitted",
        oracle.erl.display(),
        emitted.len()
    );
    assert_eq!(
        emitted.len(),
        EXPECTED.len(),
        "the oracle emitted {} cases but the embedded table has {}",
        emitted.len(),
        EXPECTED.len()
    );
    let expected = expected_map();
    let oracle_cases = case_map(&emitted, "the oracle battery");
    assert_agree(&expected, &oracle_cases, "the live oracle battery");
}

/// **The compatibility gate**: the live oracle and the live `beam_ets` must
/// produce the identical rendering for every case (except the recorded
/// divergences in [`KNOWN_DIVERGENCES`]).
#[test]
fn rust_battery_agrees_with_the_live_oracle() {
    let Some(oracle) = oracle_or_skip("rust_battery_agrees_with_the_live_oracle") else {
        return;
    };
    let oracle_cases = case_map(&run_oracle(&oracle), "the oracle battery");
    let rust_cases = case_map(&rust_battery(), "the beam_ets battery");
    eprintln!(
        "[differential] oracle: {} — comparing {} cases against beam_ets",
        oracle.erl.display(),
        oracle_cases.len()
    );
    assert_agree(&oracle_cases, &rust_cases, "the beam_ets battery");
}

/// Every recorded divergence must still be *present on both sides* and must
/// still *differ* — so the record cannot silently rot. When `beam_ets` is fixed
/// this test fails with an instruction to delete the entry (which re-enables the
/// strict comparison of that case).
#[test]
fn known_divergences_are_still_present() {
    let Some(oracle) = oracle_or_skip("known_divergences_are_still_present") else {
        return;
    };
    let oracle_cases = case_map(&run_oracle(&oracle), "the oracle battery");
    let rust_cases = case_map(&rust_battery(), "the beam_ets battery");
    for (name, explanation) in KNOWN_DIVERGENCES {
        let oracle_value = oracle_cases
            .get(*name)
            .unwrap_or_else(|| panic!("{name}: the oracle no longer emits this case"));
        let rust_value = rust_cases
            .get(*name)
            .unwrap_or_else(|| panic!("{name}: the beam_ets battery no longer emits this case"));
        assert_ne!(
            oracle_value, rust_value,
            "{name} now AGREES between erl and beam_ets — the divergence was fixed: delete it \
             from KNOWN_DIVERGENCES so the case is compared strictly again.\nRecorded reason: \
             {explanation}"
        );
        eprintln!(
            "[differential] KNOWN DIVERGENCE {name}:\n  erl:       {oracle_value}\n  beam_ets:  {rust_value}\n  reason:    {explanation}"
        );
    }
}
