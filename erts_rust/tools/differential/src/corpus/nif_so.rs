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

//! Contract **A.3** — the NIF C ABI **end to end**: a real `.so`-backed NIF round trip.
//!
//! [`super::nif`] pins the *raise* surface a NIF exposes to Erlang (`erlang:nif_error/1,2`).
//! This group pins the other half: an Erlang module `erlang:load_nif/2`s a **real shared
//! library** — C source compiled in the case's own temp dir against the host `erl_nif.h`
//! ([`crate::module_build::build`]) — and then calls the library's functions. Both runtimes
//! load *the same* `.so` and run *the same* compiled module, and only stdout + exit status
//! are compared, so a case here agrees only if the Rust runtime really binds and calls a C
//! NIF through the `enif_*` C ABI (contract A.3).
//!
//! # How the module finds its library
//!
//! A [`CaseKind::Module`] case passes integer arguments only, so the case cannot hand the
//! library path to the module. The module self-locates it instead:
//!
//! ```text
//! Dir = filename:dirname(code:which(?MODULE)),   %% the temp dir holding <module>.beam
//! Path = Dir ++ "/nif_so"                        %% the .so WITHOUT its suffix
//! ```
//!
//! The path is passed **without** the `.so` suffix: both runtimes append it, so the file
//! materialized next to the `.beam` is `<name>.so` while the module loads `<name>`. Verified
//! to work under **both** runtimes with no `-pa` and no argv change.
//!
//! # Why the module classifies instead of printing the `load_nif` result
//!
//! A failed `erlang:load_nif/2` answers `{error, {Reason, Text}}`, and `Text` embeds the
//! **absolute temp-dir path** of the library — a golden that printed it would drift on every
//! run (and the corpus requires a module-run golden to be a plain integer line). The module
//! therefore returns only the **reason tag**, mapped to an integer by `reason_code/1`:
//! `ok -> 0`, `load_failed -> 1`, `bad_lib -> 2`, `reload -> 3`, anything else `-> 9`. The
//! goldens below are the integers the **oracle** actually prints (verified per case with
//! `differential run <id>`).
//!
//! # Reachability rules this group depends on
//!
//! * `erlang:load_nif/2` must be called from the NIF module itself — the oracle refuses it
//!   through `apply/3` — so these cases are module-run cases, never `-eval` cases.
//! * every function in the library's `ErlNifFunc` table must exist **and be exported** in the
//!   Erlang module, otherwise the oracle refuses the library with
//!   `{error, {bad_lib, "Function not found nif_so:add/2"}}`.
//! * the calls into the library are **external** (`nif_so:add(2, 3)` from inside `nif_so`
//!   compiles to `call_ext_last`, not a local `call`). That matters: the Rust runtime
//!   intercepts a NIF at external dispatch, so a *local* call would keep running the
//!   `erlang:nif_error/0` stub.
//! * the C source may only use `enif_*` functions the Rust runtime exports (`enif_get_int`,
//!   `enif_make_int`, `enif_make_atom`, `enif_make_reverse_list`, `enif_make_badarg`, …) and
//!   never a C-variadic one (`enif_make_tuple`, `enif_make_list`, `enif_fprintf`,
//!   `enif_set_option`): those table slots are unpopulated, so the symbol is not exported and
//!   `dlopen(RTLD_NOW)` would fail.
//!
//! # Skip policy (contract A.3 needs host tools)
//!
//! A case whose fixture is compiled from C needs a C compiler *and* the host `erl_nif.h`;
//! when either is missing the harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`) — never a pass,
//! never a crash. `nif_so/load_missing_library` deliberately carries **no** fixture: it only
//! checks the failure path for a suffix-less path that names nothing, so it runs even where
//! no C compiler exists.
//!
//! # Honest handoff: observed divergences that are deliberately NOT cases
//!
//! A divergence would be a `mismatch` (a plain Erlang value, not a capability gap), so it is
//! reported here instead of pinned. Verified with this environment's oracle (OTP 29.1.1 /
//! `erts-17.1`) and the built `beam_rt`:
//!
//! * **a path that already ends in `.so`.** The oracle appends `.so` unconditionally, so
//!   `load_nif(<dir>/nif_so.so, 0)` becomes `nif_so.so.so` and fails (`load_failed`), while
//!   the Rust runtime de-duplicates the suffix and loads the library (`ok`). Observed:
//!   oracle `1`, Rust runtime `0` for the same module and fixture. The `.so` path rule
//!   (`beam_abi`) is the divergence, not the corpus.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded NIF module: the functions of the library's `ErlNifFunc` table, plus a
/// `classify/1` selector that drives one case each.
///
/// The stubs are dead code after a successful load (the library replaces them); they exist so
/// the oracle accepts the library at all, and they are **exported** for the same reason. Each
/// `classify/1` branch returns an integer, so the golden is a VM-stable integer line.
const NIF_MODULE_SOURCE: &str = r#"-module(nif_so).
-export([classify/1, add/2, hello/0, reverse/1, raise_badarg/0]).

%% One stub per entry in the library's ErlNifFunc table: `erlang:load_nif/2` refuses a
%% library whose functions do not exist (and are not exported) in the loading module.
add(_, _) -> erlang:nif_error(not_loaded).
hello() -> erlang:nif_error(not_loaded).
reverse(_) -> erlang:nif_error(not_loaded).
raise_badarg() -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case N of
        1 -> ok = load(Dir ++ "/nif_so"), nif_so:add(2, 3);
        2 -> ok = load(Dir ++ "/nif_so"), hd(nif_so:reverse([1, 2, 3]));
        3 -> ok = load(Dir ++ "/nif_so"), hello_code(nif_so:hello());
        4 -> ok = load(Dir ++ "/nif_so"), badarg_code();
        5 -> reason_code(load(Dir ++ "/nif_so_missing"));
        6 -> reason_code(load(Dir ++ "/nif_so_text"));
        7 -> reason_code(load(Dir ++ "/nif_so_bad"));
        8 -> ok = load(Dir ++ "/nif_so"), reason_code(load(Dir ++ "/nif_so"))
    end.

%% Only the reason *tag* leaves this function: the `Text` of a failure embeds the absolute
%% temp-dir path, which would make the golden drift on every run.
load(Path) ->
    case erlang:load_nif(Path, 0) of
        ok -> ok;
        {error, {Reason, _Text}} -> Reason;
        Other -> Other
    end.

reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.

hello_code(hello) -> 1;
hello_code(_) -> 0.

%% `enif_make_badarg` must surface as a *catchable* `error:badarg`, not as a raise that
%% escapes the module.
badarg_code() ->
    try nif_so:raise_badarg() of
        _ -> 0
    catch
        error:badarg -> 1;
        _:_ -> 2
    end.
"#;

/// The C library the round-trip cases load: a real NIF whose `ErlNifFunc` table names exactly
/// the stubs above.
///
/// It uses only `enif_*` functions the Rust runtime exports (see the module docs) and never a
/// C-variadic one — the library is loaded by *both* runtimes through `dlopen(RTLD_NOW)`, so a
/// single unresolved symbol would fail the load on the Rust side.
const REAL_NIF_C: &str = r#"/* A real NIF: the shared library the `nif_so` module loads with erlang:load_nif/2. */
#include <erl_nif.h>

/* `load` succeeds and stores no private state. */
static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    (void)env;
    (void)load_info;
    *priv = NULL;
    return 0;
}

/* add/2 -> A + B, or badarg when handed anything else. */
static ERL_NIF_TERM add(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int a, b;
    if (argc != 2 || !enif_get_int(env, argv[0], &a) || !enif_get_int(env, argv[1], &b))
        return enif_make_badarg(env);
    return enif_make_int(env, a + b);
}

/* hello/0 -> the atom `hello`. */
static ERL_NIF_TERM hello(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_atom(env, "hello");
}

/* reverse/1 -> the reversal of a proper list, badarg otherwise. */
static ERL_NIF_TERM reverse(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out = 0;
    if (argc != 1 || !enif_make_reverse_list(env, argv[0], &out))
        return enif_make_badarg(env);
    return out;
}

/* raise_badarg/0 -> raises `badarg` (the catchable failure a NIF reports). */
static ERL_NIF_TERM raise_badarg(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_badarg(env);
}

static ErlNifFunc nif_so_funcs[] = {
    {"add", 2, add, 0},
    {"hello", 0, hello, 0},
    {"reverse", 1, reverse, 0},
    {"raise_badarg", 0, raise_badarg, 0}
};

/* `ERL_NIF_INIT` seeds ErlNifEntry.name with the stringified module name (`nif_so`), so this
   library may only be loaded as `nif_so`. */
ERL_NIF_INIT(nif_so, nif_so_funcs, load, NULL, NULL, NULL)
"#;

/// A `.so` that is **not** a NIF: it compiles and links, but exports no `_nif_init`, so the
/// oracle refuses it with `{error, {bad_lib, "Failed to find library init function …"}}`.
const NOT_A_NIF_C: &str = r#"/* Not a NIF: no ERL_NIF_INIT, hence no `_nif_init` symbol to find. */
int not_a_nif(void) { return 0; }
"#;

/// A plain text file *named* `<name>.so`: the loader finds a file but cannot map it, so the
/// failure is `load_failed` without any C compiler being involved.
const TEXT_SO_CONTENTS: &str = "this file is named like a NIF library but is plain text\n";

/// The real library every round-trip case (and the `reload` case) loads.
const REAL_NIF: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_so",
    c_source: REAL_NIF_C,
};

/// Build one module-run case over [`NIF_MODULE_SOURCE`].
///
/// `selector` is the single integer argument (`classify(N)`); `nif` is the fixture the
/// harness materializes next to the compiled `.beam` (`None` when the case needs none).
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
    nif: Option<NifFixture>,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::NifAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: NIF_MODULE_SOURCE,
            module: "nif_so",
            function: "classify",
            arity: 1,
            args: selector,
            nif,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.3 end-to-end group: a real NIF round trip plus the `erlang:load_nif/2` failure shapes.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- a real NIF library: load it and call its functions ---------------------------
        case(
            "nif_so/load_and_add",
            "a real NIF .so loads and its add/2 (enif_get_int/enif_make_int) returns 5",
            &[1],
            "5",
            Some(REAL_NIF),
        ),
        case(
            "nif_so/load_and_reverse_head",
            "the NIF's reverse/1 (enif_make_reverse_list) reverses a real list",
            &[2],
            "3",
            Some(REAL_NIF),
        ),
        case(
            "nif_so/load_and_atom_result",
            "the NIF's hello/0 (enif_make_atom) returns the atom the module matches",
            &[3],
            "1",
            Some(REAL_NIF),
        ),
        case(
            "nif_so/load_and_badarg",
            "a NIF's enif_make_badarg is a catchable error:badarg, not an escaping raise",
            &[4],
            "1",
            Some(REAL_NIF),
        ),
        // --- erlang:load_nif/2 failure shapes ---------------------------------------------
        case(
            "nif_so/load_missing_library",
            "load_nif/2 on a suffix-less path that names no library is load_failed",
            &[5],
            "1",
            // No fixture: the path must *not* exist, so this case needs no C compiler at all.
            None,
        ),
        case(
            "nif_so/load_text_file_library",
            "load_nif/2 on a plain text file named <name>.so is load_failed",
            &[6],
            "1",
            Some(NifFixture::TextFileSo {
                name: "nif_so_text",
                contents: TEXT_SO_CONTENTS,
            }),
        ),
        case(
            "nif_so/load_not_a_nif_library",
            "load_nif/2 on a real .so that exports no _nif_init is bad_lib",
            &[7],
            "2",
            Some(NifFixture::CompiledLibrary {
                name: "nif_so_bad",
                c_source: NOT_A_NIF_C,
            }),
        ),
        case(
            "nif_so/load_twice_is_reload",
            "a second load_nif/2 for a module that already loaded a library is reload",
            &[8],
            "3",
            Some(REAL_NIF),
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.3 end-to-end group: every case is a module-run case claiming
    //! [`Contract::NifAbi`], it runs the shared module through `classify/1`, and its golden is
    //! the integer the oracle prints for that selector.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_nif_module_run_classifying_one_selector() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("nif_so/"),
                "case {} is outside the nif_so group",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert_eq!(
                case.contracts,
                &[Contract::NifAbi, Contract::BeamAsm],
                "case {}: an end-to-end NIF case evidences A.3 and the compiled .beam (A.2)",
                case.id
            );
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(run.module, "nif_so", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            assert_eq!(run.args.len(), 1, "case {}", case.id);
            assert!(
                selectors.insert(run.args[0]),
                "case {}: selector {} is used twice",
                case.id,
                run.args[0]
            );
            assert!(
                case.golden
                    .stdout
                    .expect("every case pins a golden")
                    .chars()
                    .all(|c| c.is_ascii_digit()),
                "case {}: the golden must be a plain integer line (a path-bearing \
                 {{error, {{Reason, Text}}}} would drift), got {:?}",
                case.id,
                case.golden.stdout
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert!(
            selectors.contains(&1) && selectors.contains(&4) && selectors.contains(&7),
            "the group must cover a successful load, a NIF raise and a bad_lib refusal"
        );
    }

    #[test]
    fn only_a_c_compiled_fixture_needs_the_c_toolchain() {
        // A case with a `CompiledLibrary` fixture needs `cc` + `erl_nif.h` (it skips loudly
        // without them); the text fixture and the no-fixture case must not, so they still run
        // on a machine with no C toolchain. Selectors 1-4 load the real library, 7 loads a
        // compiled non-NIF `.so`, and 8 reloads the real one — all need the C toolchain;
        // 5 (no fixture) and 6 (a text file named `<name>.so`) do not.
        let mut without_cc = 0;
        for case in group() {
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: expected a module-run case", case.id);
            };
            let needs_cc = matches!(run.nif, Some(NifFixture::CompiledLibrary { .. }));
            let loads_a_compiled_library = matches!(run.args[0], 1 | 2 | 3 | 4 | 7 | 8);
            assert_eq!(
                needs_cc, loads_a_compiled_library,
                "case {}: exactly the cases that load a C-compiled library need the C toolchain",
                case.id
            );
            if !needs_cc {
                without_cc += 1;
            }
        }
        assert!(
            without_cc >= 2,
            "the group must keep cases that run without a C toolchain (the missing-library and \
             text-file shapes)"
        );
    }
}
