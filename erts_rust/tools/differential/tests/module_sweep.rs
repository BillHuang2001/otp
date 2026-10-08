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

//! A **real-system-module execution sweep** through the differential harness.
//!
//! Where `src/corpus/mexec.rs` registers a small, curated set of real-`stdlib`/`kernel`
//! execution cases in the shipped corpus (so `differential suite` reports them), this
//! file is the **wide** evidence: a larger list of `(module, function, int-args)` entries
//! — deliberately bigger than the corpus group — that each exercise a **distinct real
//! OTP module** end to end under **both** runtimes, driven through
//! [`differential::harness`].
//!
//! # What the sweep proves
//!
//! For every entry a tiny wrapper module is generated (`f(A1, …) -> <the real call>`) and
//! compiled with the system `erlc`; the **oracle** applies it through `erl -pa … -eval …`
//! and the **Rust runtime** through `beam_rt run <file>.beam …`. The wrapper's call is a
//! cross-module call into the *real* library, so the Rust side must actually load and
//! execute the real system module from the code path. The two observable results
//! (stdout + exit status) are compared by the harness.
//!
//! # Honesty contract
//!
//! * **Every entry must be `Agree` or `Unsupported` — never `Mismatch`.** A mismatch is a
//!   real divergence between the runtimes and fails the test loudly; a candidate that
//!   diverges is a *finding*, not something to add with a weakened expectation.
//! * An entry the Rust runtime cannot run is `Unsupported` (its documented capability-gap
//!   signal), reported honestly and counted — never a fabricated pass.
//! * A missing oracle **skips loudly** (`SKIP: …`, never a silent pass); a missing `erlc`
//!   does the same, since the wrappers cannot be compiled without it.
//!
//! The generated wrappers are grouped by the real module they exercise, so the sweep
//! compiles one wrapper per module (not one per entry) while every entry still gets its
//! own argv and its own comparison.

use std::collections::BTreeMap;

use differential::harness::{run_case_in, RuntimeContext};
use differential::module_build;
use differential::runtime::resolve_oracle;
use differential::{Case, CaseKind, Contract, Golden, ModuleRun, Verdict};

mod common;

/// One sweep entry: a real system module + function invoked with integer arguments.
struct Entry {
    /// The real system module exercised (also the generated wrapper's group key).
    module: &'static str,
    /// `module:function/arity` — the human label printed per entry.
    spec: &'static str,
    /// The integer arguments handed to the wrapper function (and so to the real call).
    args: &'static [i64],
    /// The wrapper body, referencing the parameters as `A1`, `A2`, …; it makes the real
    /// cross-module call (and any list/tuple the real function needs).
    body: &'static str,
}

/// The contracts every sweep entry evidences: a compiled `.beam` executes (A.2) and a
/// real OTP application module is loaded and run (A.7).
const CONTRACTS: &[Contract] = &[Contract::BeamAsm, Contract::OtpApps];

/// The curated sweep: real modules `calendar`, `ordsets`, `queue`, `gb_trees`, `gb_sets`,
/// `proplists`, `orddict`, `array`, `string`, `unicode`, `io_lib`, `filename`,
/// `erl_scan`, `base64`.
///
/// Every entry here is verified `Agree` against the current `beam_rt`. Modules that are a
/// *genuine* runtime gap today (`sets`, `math`, `dict`, `string:to_integer/1`) are
/// deliberately absent: they raise in the Rust runtime where the oracle returns a value,
/// which is a `mismatch` — reported, not claimed.
const SWEEP: &[Entry] = &[
    // --- calendar ---------------------------------------------------------------
    Entry { module: "calendar", spec: "calendar:last_day_of_the_month/2", args: &[2024, 2], body: "calendar:last_day_of_the_month(A1, A2)" },
    Entry { module: "calendar", spec: "calendar:last_day_of_the_month/2", args: &[2000, 2], body: "calendar:last_day_of_the_month(A1, A2)" },
    Entry { module: "calendar", spec: "calendar:day_of_the_week/3", args: &[2000, 1, 1], body: "calendar:day_of_the_week(A1, A2, A3)" },
    Entry { module: "calendar", spec: "calendar:day_of_the_week/3", args: &[2024, 2, 29], body: "calendar:day_of_the_week(A1, A2, A3)" },
    Entry { module: "calendar", spec: "calendar:date_to_gregorian_days/3", args: &[2000, 1, 1], body: "calendar:date_to_gregorian_days(A1, A2, A3)" },
    Entry { module: "calendar", spec: "calendar:date_to_gregorian_days/3", args: &[2024, 2, 29], body: "calendar:date_to_gregorian_days(A1, A2, A3)" },
    Entry { module: "calendar", spec: "calendar:is_leap_year/1", args: &[2024], body: "case calendar:is_leap_year(A1) of true -> 1; false -> 0 end" },
    Entry { module: "calendar", spec: "calendar:is_leap_year/1", args: &[2023], body: "case calendar:is_leap_year(A1) of true -> 1; false -> 0 end" },
    Entry { module: "calendar", spec: "calendar:gregorian_days_to_date/1", args: &[730485], body: "calendar:gregorian_days_to_date(A1)" },
    Entry { module: "calendar", spec: "calendar:valid_date/3", args: &[2024, 2, 29], body: "case calendar:valid_date(A1, A2, A3) of true -> 1; false -> 0 end" },
    Entry { module: "calendar", spec: "calendar:valid_date/3", args: &[2023, 2, 29], body: "case calendar:valid_date(A1, A2, A3) of true -> 1; false -> 0 end" },
    // --- ordsets (calls into lists) --------------------------------------------
    Entry { module: "ordsets", spec: "ordsets:from_list/1", args: &[3, 1, 2], body: "length(ordsets:from_list([A1, A2, A2, A3]))" },
    Entry { module: "ordsets", spec: "ordsets:add_element/2", args: &[4], body: "length(ordsets:add_element(A1, ordsets:from_list([1, 2, 3])))" },
    Entry { module: "ordsets", spec: "ordsets:is_element/2", args: &[3], body: "case ordsets:is_element(A1, ordsets:from_list([1, 3, 5])) of true -> 1; false -> 0 end" },
    Entry { module: "ordsets", spec: "ordsets:union/2", args: &[7], body: "length(ordsets:union(ordsets:from_list([1, 2]), ordsets:from_list([2, A1])))" },
    Entry { module: "ordsets", spec: "ordsets:intersection/2", args: &[3], body: "length(ordsets:intersection(ordsets:from_list([1, 2, 3]), ordsets:from_list([2, 3, A1])))" },
    Entry { module: "ordsets", spec: "ordsets:subtract/2", args: &[2], body: "length(ordsets:subtract(ordsets:from_list([1, 2, 3]), ordsets:from_list([A1])))" },
    // --- queue -----------------------------------------------------------------
    Entry { module: "queue", spec: "queue:from_list/1", args: &[5, 6, 7], body: "queue:len(queue:from_list([A1, A2, A3]))" },
    Entry { module: "queue", spec: "queue:in/2", args: &[4], body: "queue:len(queue:in(A1, queue:from_list([1, 2, 3])))" },
    Entry { module: "queue", spec: "queue:out/1", args: &[7], body: "queue:len(element(2, queue:out(queue:from_list([A1, 9, 8]))))" },
    Entry { module: "queue", spec: "queue:out/1", args: &[7], body: "element(2, element(1, queue:out(queue:from_list([A1, 9, 8]))))" },
    Entry { module: "queue", spec: "queue:join/2", args: &[4], body: "queue:len(queue:join(queue:from_list([1, 2]), queue:from_list([3, A1])))" },
    Entry { module: "queue", spec: "queue:to_list/1", args: &[1, 2], body: "queue:to_list(queue:from_list([A1, A2]))" },
    // --- gb_trees --------------------------------------------------------------
    Entry { module: "gb_trees", spec: "gb_trees:size/1", args: &[5], body: "gb_trees:size(lists:foldl(fun(X, T) -> gb_trees:insert(X, X * 10, T) end, gb_trees:empty(), lists:seq(1, A1)))" },
    Entry { module: "gb_trees", spec: "gb_trees:get/2", args: &[3], body: "gb_trees:get(A1, gb_trees:insert(A1, A1 * 10, gb_trees:empty()))" },
    Entry { module: "gb_trees", spec: "gb_trees:is_defined/2", args: &[1], body: "case gb_trees:is_defined(A1, gb_trees:insert(1, 1, gb_trees:empty())) of true -> 1; false -> 0 end" },
    Entry { module: "gb_trees", spec: "gb_trees:smallest/1", args: &[2], body: "element(1, gb_trees:smallest(gb_trees:insert(A1, 1, gb_trees:insert(5, 2, gb_trees:empty()))))" },
    Entry { module: "gb_trees", spec: "gb_trees:largest/1", args: &[9], body: "element(1, gb_trees:largest(gb_trees:insert(A1, 1, gb_trees:insert(5, 2, gb_trees:empty()))))" },
    Entry { module: "gb_trees", spec: "gb_trees:lookup/2", args: &[1], body: "gb_trees:lookup(A1, gb_trees:insert(1, 11, gb_trees:empty()))" },
    Entry { module: "gb_trees", spec: "gb_trees:keys/1", args: &[4], body: "length(gb_trees:keys(lists:foldl(fun(X, T) -> gb_trees:insert(X, X, T) end, gb_trees:empty(), lists:seq(1, A1))))" },
    // --- gb_sets ---------------------------------------------------------------
    Entry { module: "gb_sets", spec: "gb_sets:size/1", args: &[4], body: "gb_sets:size(gb_sets:from_list([1, 2, 2, A1]))" },
    Entry { module: "gb_sets", spec: "gb_sets:add_element/2", args: &[4], body: "gb_sets:size(gb_sets:add_element(A1, gb_sets:from_list([1, 2, 3])))" },
    Entry { module: "gb_sets", spec: "gb_sets:is_element/2", args: &[3], body: "case gb_sets:is_element(A1, gb_sets:from_list([1, 3, 5])) of true -> 1; false -> 0 end" },
    Entry { module: "gb_sets", spec: "gb_sets:smallest/1", args: &[2], body: "gb_sets:smallest(gb_sets:from_list([A1, 9, 8]))" },
    Entry { module: "gb_sets", spec: "gb_sets:to_list/1", args: &[3], body: "length(gb_sets:to_list(gb_sets:from_list([1, 2, A1])))" },
    // --- proplists -------------------------------------------------------------
    Entry { module: "proplists", spec: "proplists:get_value/3", args: &[2, 0], body: "proplists:get_value(A1, [{1, 10}, {2, 20}, {3, 30}], A2)" },
    Entry { module: "proplists", spec: "proplists:get_value/3", args: &[9, 77], body: "proplists:get_value(A1, [{1, 10}, {2, 20}], A2)" },
    Entry { module: "proplists", spec: "proplists:is_defined/2", args: &[2], body: "case proplists:is_defined(A1, [{1, x}, {2, y}]) of true -> 1; false -> 0 end" },
    Entry { module: "proplists", spec: "proplists:get_all_values/2", args: &[1], body: "length(proplists:get_all_values(A1, [{1, a}, {1, b}, {2, c}]))" },
    Entry { module: "proplists", spec: "proplists:lookup/2", args: &[1], body: "proplists:lookup(A1, [{1, a}, {2, b}])" },
    // --- orddict ---------------------------------------------------------------
    Entry { module: "orddict", spec: "orddict:fetch/2", args: &[2, 0], body: "orddict:fetch(A1, orddict:from_list([{1, 10}, {2, 20}])) + (A2 - A2)" },
    Entry { module: "orddict", spec: "orddict:size/1", args: &[9], body: "orddict:size(orddict:from_list([{1, a}, {2, b}, {3, c}])) + (A1 - A1)" },
    Entry { module: "orddict", spec: "orddict:is_key/2", args: &[1], body: "case orddict:is_key(A1, orddict:from_list([{1, a}])) of true -> 1; false -> 0 end" },
    Entry { module: "orddict", spec: "orddict:store/3", args: &[2, 7], body: "orddict:size(orddict:store(A1, A2, orddict:from_list([{1, a}])))" },
    Entry { module: "orddict", spec: "orddict:find/2", args: &[1], body: "orddict:find(A1, orddict:from_list([{1, a}]))" },
    // --- array -----------------------------------------------------------------
    Entry { module: "array", spec: "array:size/1", args: &[4], body: "array:size(array:from_list(lists:seq(1, A1)))" },
    Entry { module: "array", spec: "array:get/2", args: &[11], body: "array:get(1, array:from_list([A1, 20, 30]))" },
    Entry { module: "array", spec: "array:get/2", args: &[2, 7], body: "array:get(A1, array:from_list(lists:duplicate(5, A2)))" },
    Entry { module: "array", spec: "array:set/3", args: &[9], body: "array:size(array:set(1, A1, array:from_list([1, 2, 3])))" },
    Entry { module: "array", spec: "array:to_list/1", args: &[2], body: "array:to_list(array:from_list([1, A1, 3]))" },
    // --- string (a unicode consumer) -------------------------------------------
    Entry { module: "string", spec: "string:length/1", args: &[4], body: "string:length(lists:duplicate(A1, $a))" },
    Entry { module: "string", spec: "string:uppercase/1", args: &[0], body: "hd(string:uppercase([$a + (A1 rem 26)]))" },
    Entry { module: "string", spec: "string:trim/1", args: &[3], body: "string:length(string:trim(lists:duplicate(A1, $\\s) ++ \"x\"))" },
    Entry { module: "string", spec: "string:reverse/1", args: &[1234], body: "hd(string:reverse(integer_to_list(A1)))" },
    Entry { module: "string", spec: "string:slice/2", args: &[5], body: "string:length(string:slice(lists:duplicate(A1, $a) ++ \"xyz\", 2))" },
    // --- unicode ---------------------------------------------------------------
    Entry { module: "unicode", spec: "unicode:characters_to_list/1", args: &[12345], body: "length(unicode:characters_to_list(integer_to_list(A1)))" },
    Entry { module: "unicode", spec: "unicode:characters_to_binary/1", args: &[42], body: "byte_size(unicode:characters_to_binary(integer_to_list(A1)))" },
    Entry { module: "unicode", spec: "unicode round-trip", args: &[7], body: "byte_size(unicode:characters_to_binary(unicode:characters_to_list(integer_to_list(A1))))" },
    // --- io_lib ----------------------------------------------------------------
    Entry { module: "io_lib", spec: "io_lib:format/2 (flattened)", args: &[12345], body: "length(lists:flatten(io_lib:format(\"~w\", [A1])))" },
    Entry { module: "io_lib", spec: "io_lib:format/2 (~p)", args: &[7], body: "hd(io_lib:format(\"~p\", [A1]))" },
    Entry { module: "io_lib", spec: "io_lib:format/2 (iolist_size)", args: &[12345], body: "iolist_size(io_lib:format(\"~w\", [A1]))" },
    Entry { module: "io_lib", spec: "io_lib:format/2 (flatten)", args: &[42], body: "lists:flatten(io_lib:format(\"~w\", [A1]))" },
    // --- filename --------------------------------------------------------------
    Entry { module: "filename", spec: "filename:basename/1", args: &[42], body: "hd(filename:basename(integer_to_list(A1) ++ \".txt\"))" },
    Entry { module: "filename", spec: "filename:extension/1", args: &[9], body: "length(filename:extension(\"file\" ++ integer_to_list(A1) ++ \".ext\"))" },
    Entry { module: "filename", spec: "filename:join/1", args: &[42], body: "length(filename:join([\"/tmp\", integer_to_list(A1)]))" },
    Entry { module: "filename", spec: "filename:basename/1 (length)", args: &[12345], body: "length(filename:basename(integer_to_list(A1)))" },
    Entry { module: "filename", spec: "filename:rootname/1", args: &[9], body: "filename:rootname(\"file\" ++ integer_to_list(A1) ++ \".ext\")" },
    // --- erl_scan --------------------------------------------------------------
    Entry { module: "erl_scan", spec: "erl_scan:string/1", args: &[42], body: "length(element(2, erl_scan:string(integer_to_list(A1))))" },
    Entry { module: "erl_scan", spec: "erl_scan:string/1 (expression)", args: &[7], body: "length(element(2, erl_scan:string(integer_to_list(A1) ++ \" + 1\")))" },
    // --- base64 ----------------------------------------------------------------
    Entry { module: "base64", spec: "base64:encode/1", args: &[5], body: "byte_size(base64:encode(list_to_binary(lists:seq(1, A1))))" },
    Entry { module: "base64", spec: "base64:encode/1", args: &[65], body: "byte_size(base64:encode(<<A1>>))" },
    Entry { module: "base64", spec: "base64:decode/1", args: &[200], body: "byte_size(base64:decode(base64:encode(<<A1>>)))" },
];

/// The Erlang parameter list for an `arity`-argument wrapper function.
fn params(arity: usize) -> String {
    (1..=arity)
        .map(|index| format!("A{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Generate, group and leak one wrapper module per distinct real module.
///
/// The returned map is keyed by the real module name; each value is the source of a
/// `sweep_<module>` wrapper exporting one `f<entry-index>` per entry of that module. Only
/// the source (and, in [`realize`], the module/function names) is leaked — the sweep is a
/// single, bounded test process, so the leaked strings are reclaimed at process exit.
fn wrapper_sources() -> BTreeMap<&'static str, &'static str> {
    let mut by_module: BTreeMap<&'static str, Vec<usize>> = BTreeMap::new();
    for (index, entry) in SWEEP.iter().enumerate() {
        by_module.entry(entry.module).or_default().push(index);
    }

    let mut sources = BTreeMap::new();
    for (module, indices) in by_module {
        let mut source = format!("-module(sweep_{module}).\n");
        let exports: Vec<String> = indices
            .iter()
            .map(|&index| format!("f{index}/{}", SWEEP[index].args.len()))
            .collect();
        source.push_str(&format!("-export([{}]).\n", exports.join(", ")));
        for &index in &indices {
            let entry = &SWEEP[index];
            source.push_str(&format!(
                "f{index}({}) -> {}.\n",
                params(entry.args.len()),
                entry.body
            ));
        }
        let leaked: &'static str = Box::leak(source.into_boxed_str());
        sources.insert(module, leaked);
    }
    sources
}

/// Build the harness [`Case`] for one sweep entry, resolving the name of its generated
/// wrapper module + function.
fn case_for(index: usize, entry: &Entry, sources: &BTreeMap<&'static str, &'static str>) -> Case {
    let module: &'static str = Box::leak(format!("sweep_{}", entry.module).into_boxed_str());
    let function: &'static str = Box::leak(format!("f{index}").into_boxed_str());
    Case {
        id: "module-sweep",
        description: "a real system module runs end to end under both runtimes",
        contracts: CONTRACTS,
        kind: CaseKind::Module(ModuleRun {
            source: sources[entry.module],
            module,
            function,
            arity: entry.args.len() as u32,
            args: entry.args,
            // The sweep runs plain system modules: no C/NIF fixture (contract A.3) is needed.
            nif: None,
        }),
        // No stdout golden: the sweep records *agreement*, not a value. The oracle is only
        // required to exit 0 (a well-formed entry); a non-zero oracle exit is corpus drift.
        golden: Golden {
            stdout: None,
            exit_code: 0,
        },
    }
}

/// The stdout of an observed side, trimmed, for the per-entry report.
fn observed_stdout(side: &differential::SideResult) -> String {
    match side.observation() {
        Some(observation) => String::from_utf8_lossy(&observation.stdout)
            .trim_end()
            .to_string(),
        None => String::new(),
    }
}

#[test]
fn real_module_execution_sweep_never_mismatches() {
    // The sweep must be wider than the shipped corpus group it mirrors.
    assert!(
        SWEEP.len() > 30,
        "the sweep must be larger than the corpus mexec group (got {} entries)",
        SWEEP.len()
    );

    if let Err(error) = resolve_oracle() {
        eprintln!("SKIP: module sweep — no oracle available ({error})");
        return;
    }
    match module_build::resolve_erlc() {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("SKIP: module sweep — no erlc available (the wrappers cannot be compiled)");
            return;
        }
        Err(error) => {
            eprintln!("SKIP: module sweep — erlc resolution failed ({error})");
            return;
        }
    }

    // An explicit context so the sweep observes the freshly built debug `beam_rt` rather
    // than the (stale-prone) `release`-first workspace default.
    let context = RuntimeContext {
        oracle: resolve_oracle().expect("the oracle resolves (checked above)"),
        rust: common::fresh_rust_spec(),
    };
    let sources = wrapper_sources();

    let mut agree = 0usize;
    let mut unsupported = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    // Per real module: (agree, unsupported).
    let mut per_module: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();

    for (index, entry) in SWEEP.iter().enumerate() {
        let case = case_for(index, entry, &sources);
        let outcome = run_case_in(&context, &case).expect("the harness runs a sweep case");
        let counts = per_module.entry(entry.module).or_insert((0, 0));
        match outcome.verdict {
            Verdict::Agree => {
                agree += 1;
                counts.0 += 1;
                println!(
                    "[agree]       {} {:?} -> {}",
                    entry.spec,
                    entry.args,
                    observed_stdout(&outcome.oracle)
                );
            }
            Verdict::Unsupported => {
                unsupported += 1;
                counts.1 += 1;
                let reason = outcome
                    .rust
                    .unsupported_reason()
                    .unwrap_or("(no reason)");
                println!(
                    "[unsupported] {} {:?} — {reason}",
                    entry.spec, entry.args
                );
            }
            other => {
                // A real divergence (or a skip the pre-checks did not foresee) is loud.
                mismatches.push(format!(
                    "{} {:?}: oracle=<{}> rust=<{}> verdict={} diffs={:?}",
                    entry.spec,
                    entry.args,
                    observed_stdout(&outcome.oracle),
                    observed_stdout(&outcome.rust),
                    other.as_str(),
                    outcome.diffs,
                ));
            }
        }
    }

    println!();
    for (module, (a, u)) in &per_module {
        println!("  {module:<12} {a} agree, {u} unsupported");
    }
    println!(
        "SWEEP: {} entries across {} real modules — {agree} agree, {unsupported} unsupported, {} not-agreeing",
        SWEEP.len(),
        per_module.len(),
        mismatches.len()
    );

    assert!(
        mismatches.is_empty(),
        "the module sweep must never mismatch — {} entry/entries diverged or could not run:\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );
}
