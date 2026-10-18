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
//! `ok -> 0`, `load_failed -> 1`, `bad_lib -> 2`, `reload -> 3`, `load -> 4`, anything else
//! `-> 9`. The goldens below are the integers the **oracle** actually prints (verified per case
//! with `differential run <id>`).
//!
//! The refusals whose message carries **no path** are pinned one step further, by
//! `fail_code/1`: `reason_code * 10 + text_class`, where `text_class/2` is `1` only when the
//! `Text` equals the word-for-word string the oracle composes (`erl_nif.c`) and `0` otherwise.
//! Each such golden therefore pins the reason **and** the exact message class (e.g. `21` =
//! `bad_lib` with the expected text), while still being a plain integer line: a reworded
//! runtime shows up as a drift instead of a silent pass.
//!
//! # What this group covers
//!
//! * the **call** path of a real library — `add/2` (`enif_get_int`/`enif_make_int`),
//!   `reverse/1` (`enif_make_reverse_list`), `hello/0` (`enif_make_atom`) and a NIF raise
//!   (`enif_make_badarg`) — and the `LoadInfo` hand-off (`load_info_reaches_the_load_callback`:
//!   the term the module passes reaches the library's `load` callback verbatim);
//! * the **`erlang:load_nif/2` path-type contract** (`non_string_path_is_a_catchable_badarg`):
//!   a non-string path is a catchable `badarg`, a binary path reaches the loader;
//! * the **loader refusal shapes**: a missing library, a plain text file named `<name>.so`, a
//!   `.so` without `_nif_init`, a second load of the same module (`reload`), a **`load`
//!   callback veto**, an entry naming **another module**, and the three **entry-header**
//!   refusals a hand-written `nif_init` can provoke — version too old (`2.5`), version too new
//!   (with its `min_erts`) and a foreign `vm_variant`.
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
//!   `enif_make_int`, `enif_make_atom`, `enif_make_reverse_list`, `enif_make_badarg`,
//!   `enif_get_tuple`, `enif_is_identical`, …) and never a C-variadic one (`enif_make_tuple`,
//!   `enif_make_list`, `enif_fprintf`, `enif_set_option`): those table slots are unpopulated,
//!   so the symbol is not exported and `dlopen(RTLD_NOW)` would fail.
//! * the hand-written-entry fixtures export their own `nif_init` (the symbol
//!   `erts_sys_ddll_load_nif_init` resolves) instead of using `ERL_NIF_INIT`, so they can
//!   declare an `ErlNifEntry` the macros would never produce.
//!
//! # Skip policy (contract A.3 needs host tools)
//!
//! A case whose fixture is compiled from C needs a C compiler *and* the host `erl_nif.h`;
//! when either is missing the harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`) — never a pass,
//! never a crash. Two cases deliberately carry **no** fixture: `nif_so/load_missing_library`
//! (the failure path for a suffix-less path that names nothing) and
//! `nif_so/non_string_path_is_a_catchable_badarg` (the path-type contract), so they run even
//! where no C compiler exists.
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
//! * **the function-table checks after the name check** — `beam_abi`'s loader performs the
//!   entry's version / variant / module-name validation but not the four checks the C runs
//!   against the *calling module's* code (it cannot reach that code). Measured with a scratch
//!   module against this host: a library declaring a function the module does not export
//!   (`Function not found nif_gap:add/3`) and one declaring an out-of-range `flags` field
//!   (`Illegal flags field value 99 for NIF nif_gap:notify/0`) are both refused by the oracle
//!   with `bad_lib` while the Rust runtime answers `ok` — so no case pins either (a pinned one
//!   would be a `mismatch`). (A library declaring a function whose Erlang body is a plain
//!   clause instead of an `erlang:nif_error/1` stub is *accepted* by both.) Owning crates:
//!   `beam_abi` (the loader, which would need the module's code) and `beam_rt` (which has it
//!   and could hand it over).
use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded NIF module: the functions of the library's `ErlNifFunc` table, plus a
/// `classify/1` selector that drives one case each.
///
/// The stubs are dead code after a successful load (the library replaces them); they exist so
/// the oracle accepts the library at all, and they are **exported** for the same reason. Each
/// `classify/1` branch returns an integer, so the golden is a VM-stable integer line.
const NIF_MODULE_SOURCE: &str = r#"-module(nif_so).
-export([classify/1, add/2, hello/0, reverse/1, raise_badarg/0, load_info_probe/0]).

%% One stub per entry in the library's ErlNifFunc table: `erlang:load_nif/2` refuses a
%% library whose functions do not exist (and are not exported) in the loading module.
add(_, _) -> erlang:nif_error(not_loaded).
hello() -> erlang:nif_error(not_loaded).
reverse(_) -> erlang:nif_error(not_loaded).
raise_badarg() -> erlang:nif_error(not_loaded).
load_info_probe() -> erlang:nif_error(not_loaded).

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
        8 -> ok = load(Dir ++ "/nif_so"), reason_code(load(Dir ++ "/nif_so"));
        %% The refusals below keep the `Text` as well: every one of them is path-free, so
        %% `fail_code/1` pins the exact message class instead of only the reason tag.
        9 -> fail_code(erlang:load_nif(Dir ++ "/nif_so_veto", 0));
        10 -> fail_code(erlang:load_nif(Dir ++ "/nif_so_othername", 0));
        11 -> fail_code(erlang:load_nif(Dir ++ "/nif_so_oldver", 0));
        12 -> fail_code(erlang:load_nif(Dir ++ "/nif_so_newver", 0));
        13 -> fail_code(erlang:load_nif(Dir ++ "/nif_so_variant", 0));
        %% The `LoadInfo` term reaches the library's `load` callback verbatim: the callback
        %% recorded whether it was handed exactly the tuple this branch passes.
        14 -> ok = erlang:load_nif(Dir ++ "/nif_so", {nif_so_load_info, 7}),
              nif_so:load_info_probe();
        %% The path argument's own type contract (no library involved).
        15 -> path_type_codes(Dir)
    end.
%% Only the reason *tag* leaves this function: the `Text` of these failures embeds the absolute
%% temp-dir path, which would make the golden drift on every run.
load(Path) ->
    case erlang:load_nif(Path, 0) of
        ok -> ok;
        {error, {Reason, _Text}} -> Reason;
        Other -> Other
    end.

%% Reduce the whole `{error, {Reason, Text}}` result to one integer — `reason_code * 10` plus
%% `text_class` — so a case pins the reason *and* the message class while the golden stays a
%% plain integer line.
fail_code(ok) -> 0;
fail_code({error, {Reason, Text}}) ->
    reason_code(Reason) * 10 + text_class(Reason, iolist_to_binary(Text));
fail_code(_) -> 900.

%% `erlang:load_nif/2`'s path argument: anything that is neither a byte-sized character list nor
%% a binary is a *catchable* `badarg` (never a `load_failed`), while a binary path is accepted and
%% reaches the loader — which then fails to open the missing library. Returns
%% 1 (integer path) + 2 (tuple path) + 10 (binary path -> load_failed) = 13.
path_type_codes(Dir) ->
    catch_badarg(123) + catch_badarg({nif_so, path}) * 2 + binary_path_code(Dir) * 10.

catch_badarg(Path) ->
    try erlang:load_nif(Path, 0) of
        _ -> 0
    catch
        error:badarg -> 1;
        _:_ -> 0
    end.

binary_path_code(Dir) ->
    case erlang:load_nif(list_to_binary(Dir ++ "/nif_so_missing"), 0) of
        {error, {Reason, _Text}} -> reason_code(Reason);
        _ -> 900
    end.
%% The exact `Text` of a path-free refusal, as the oracle words it (`erl_nif.c`): 1 when it
%% matches, 0 when it does not — so a reworded runtime is a visible drift, never a silent pass.
text_class(bad_lib, <<"Library module name 'nif_so_other' does not match calling module 'nif_so'">>) -> 1;
text_class(bad_lib, <<"That old NIF library (2.5) is not compatible with this erts runtime (2.18). Try recompile the NIF lib.">>) -> 1;
text_class(bad_lib, <<"That 'nif_so' NIF library needs erts-99.0 or newer. Either try to recompile the NIF lib or use a newer erts runtime.">>) -> 1;
text_class(bad_lib, <<"Library (erl_prim.example) not compiled for this vm variant (beam.vanilla).">>) -> 1;
text_class(load, <<"Library load-call unsuccessful (1).">>) -> 1;
text_class(_, _) -> 0.

reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(load) -> 4;
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

/* Whether the load callback was handed exactly the `{nif_so_load_info, 7}` tuple, i.e. whether
   the `LoadInfo` argument of `erlang:load_nif/2` reaches the library verbatim. */
static int load_info_seen = 0;

/* `load` succeeds, stores no private state and records the `load_info` term it was handed. */
static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    const ERL_NIF_TERM* tup;
    int arity = 0, seven = 0;
    *priv = NULL;
    load_info_seen = 0;
    if (enif_get_tuple(env, load_info, &arity, &tup) && arity == 2 &&
        enif_is_identical(enif_make_atom(env, "nif_so_load_info"), tup[0]) &&
        enif_get_int(env, tup[1], &seven) && seven == 7)
        load_info_seen = 1;
    return 0;
}

/* The recorded `load_info` observation: 1 when the callback saw the expected tuple. */
static ERL_NIF_TERM load_info_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_int(env, load_info_seen);
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
    {"raise_badarg", 0, raise_badarg, 0},
    {"load_info_probe", 0, load_info_probe, 0}
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

/// A real NIF whose **`load` callback vetoes**: the entry is named `nif_so` (so the module-name
/// check passes) and its function table names the module's own stubs (so the function-table checks
/// pass), but `load` returns a nonzero code. The oracle then refuses the library with the
/// `load` reason (`{error, {load, "Library load-call unsuccessful (1)."}}`).
///
/// The file is named `nif_so_veto` while its entry is named `nif_so`: `ERL_NIF_INIT`'s first
/// argument only stringifies into `ErlNifEntry.name`, so the stem of the file the module loads and
/// the module name the entry must match are independent.
const VETO_NIF_C: &str = r#"/* A NIF whose load callback vetoes the load. */
#include <erl_nif.h>

/* The function table must name the module's existing, exported stubs — the veto is the *only*
   reason this library is refused. The pointers are never called. */
static ERL_NIF_TERM noop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_badarg(env);
}

static int veto_load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    (void)env;
    (void)priv;
    (void)load_info;
    return 1;   /* nonzero: refuse the library */
}

static ErlNifFunc veto_funcs[] = {
    {"add", 2, noop, 0},
    {"hello", 0, noop, 0},
    {"reverse", 1, noop, 0},
    {"raise_badarg", 0, noop, 0},
    {"load_info_probe", 0, noop, 0}
};

ERL_NIF_INIT(nif_so, veto_funcs, veto_load, NULL, NULL, NULL)
"#;

/// A real NIF whose **entry names another module**: `ERL_NIF_INIT(nif_so_other, …)` sets
/// `ErlNifEntry.name` to `nif_so_other`, which the loader compares against the *calling* module
/// (`nif_so`) before it looks at anything else. The oracle refuses it with
/// `{error, {bad_lib, "Library module name 'nif_so_other' does not match calling module 'nif_so'"}}`.
const OTHER_NAME_NIF_C: &str = r#"/* A NIF whose entry names a module other than the one loading it. */
#include <erl_nif.h>

static ERL_NIF_TERM noop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_badarg(env);
}

static ErlNifFunc other_funcs[] = {{"add", 2, noop, 0}};

ERL_NIF_INIT(nif_so_other, other_funcs, NULL, NULL, NULL, NULL)
"#;

/// The **entry contract** without `ERL_NIF_INIT`: the library exports the `nif_init` symbol the
/// loader looks for and answers its own `ErlNifEntry`. This one declares a version **older** than
/// this runtime supports (`2.5`, the experimental-maps release), which `erts` refuses with
/// `{error, {bad_lib, "That old NIF library (2.5) is not compatible with this erts runtime
/// (2.18). Try recompile the NIF lib."}}` — before it ever looks at the name or the function
/// table, which is why the entry carries neither.
const OLD_VERSION_NIF_C: &str = r#"/* The entry contract by hand: a NIF entry with an unsupported (too old) version. */
#include <erl_nif.h>

static ErlNifEntry old_entry = {
    .major = 2,
    .minor = 5,
    .name = "nif_so",
    .num_of_funcs = 0,
    .funcs = 0,
    .load = 0,
    .reload = 0,
    .upgrade = 0,
    .unload = 0,
    .vm_variant = ERL_NIF_VM_VARIANT,
    .options = 0,
    .sizeof_ErlNifResourceTypeInit = sizeof(ErlNifResourceTypeInit),
    .min_erts = "erts-99.0"
};

ErlNifEntry* nif_init(void) { return &old_entry; }
"#;

/// A hand-written entry whose version is **newer** than this runtime supports, so the loader
/// refuses it with the `min_erts` message it carries:
/// `{error, {bad_lib, "That 'nif_so' NIF library needs erts-99.0 or newer. …"}}`.
const NEW_VERSION_NIF_C: &str = r#"/* The entry contract by hand: a NIF entry with an unsupported (too new) version. */
#include <erl_nif.h>

static ErlNifEntry new_entry = {
    .major = 2,
    .minor = 99,
    .name = "nif_so",
    .num_of_funcs = 0,
    .funcs = 0,
    .load = 0,
    .reload = 0,
    .upgrade = 0,
    .unload = 0,
    .vm_variant = ERL_NIF_VM_VARIANT,
    .options = 0,
    .sizeof_ErlNifResourceTypeInit = sizeof(ErlNifResourceTypeInit),
    .min_erts = "erts-99.0"
};

ErlNifEntry* nif_init(void) { return &new_entry; }
"#;

/// A hand-written entry with a supported version but a **foreign vm variant**, which the loader
/// refuses with `{error, {bad_lib, "Library (erl_prim.example) not compiled for this vm variant
/// (beam.vanilla)."}}`.
const WRONG_VARIANT_NIF_C: &str = r#"/* The entry contract by hand: a NIF entry compiled for another vm variant. */
#include <erl_nif.h>

static ErlNifEntry variant_entry = {
    .major = 2,
    .minor = ERL_NIF_MINOR_VERSION,
    .name = "nif_so",
    .num_of_funcs = 0,
    .funcs = 0,
    .load = 0,
    .reload = 0,
    .upgrade = 0,
    .unload = 0,
    .vm_variant = "erl_prim.example",
    .options = 0,
    .sizeof_ErlNifResourceTypeInit = sizeof(ErlNifResourceTypeInit),
    .min_erts = "erts-99.0"
};

ErlNifEntry* nif_init(void) { return &variant_entry; }
"#;
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
        // --- the load-callback and entry-contract refusals (reason *and* message class) -----
        case(
            "nif_so/load_callback_veto",
            "a library whose load callback returns nonzero is refused with the `load` reason",
            &[9],
            // reason `load` (4) * 10 + the exact `Library load-call unsuccessful (1).` text (1).
            "41",
            Some(NifFixture::CompiledLibrary {
                name: "nif_so_veto",
                c_source: VETO_NIF_C,
            }),
        ),
        case(
            "nif_so/load_library_name_mismatch",
            "a library whose entry names another module is refused with bad_lib and the name-mismatch text",
            &[10],
            // reason `bad_lib` (2) * 10 + the exact name-mismatch text (1).
            "21",
            Some(NifFixture::CompiledLibrary {
                name: "nif_so_othername",
                c_source: OTHER_NAME_NIF_C,
            }),
        ),
        case(
            "nif_so/load_entry_version_too_old",
            "a hand-written entry header with version 2.5 is refused with bad_lib and the too-old text",
            &[11],
            // reason `bad_lib` (2) * 10 + the exact "(2.5) … (2.18)" text (1).
            "21",
            Some(NifFixture::CompiledLibrary {
                name: "nif_so_oldver",
                c_source: OLD_VERSION_NIF_C,
            }),
        ),
        case(
            "nif_so/load_entry_version_too_new",
            "a hand-written entry header newer than the runtime is refused with bad_lib and its min_erts text",
            &[12],
            // reason `bad_lib` (2) * 10 + the exact "needs erts-99.0 or newer" text (1).
            "21",
            Some(NifFixture::CompiledLibrary {
                name: "nif_so_newver",
                c_source: NEW_VERSION_NIF_C,
            }),
        ),
        case(
            "nif_so/load_entry_wrong_vm_variant",
            "a hand-written entry compiled for a foreign vm variant is refused with bad_lib and the variant text",
            &[13],
            // reason `bad_lib` (2) * 10 + the exact "not compiled for this vm variant" text (1).
            "21",
            Some(NifFixture::CompiledLibrary {
                name: "nif_so_variant",
                c_source: WRONG_VARIANT_NIF_C,
            }),
        ),
        // --- the LoadInfo hand-off ---------------------------------------------------------
        case(
            "nif_so/load_info_reaches_the_load_callback",
            "the LoadInfo term of erlang:load_nif/2 reaches the library's load callback verbatim",
            &[14],
            // The callback saw exactly `{nif_so_load_info, 7}` and the module read its own NIF back.
            "1",
            Some(REAL_NIF),
        ),
        // --- the path argument's type contract (no library needed) --------------------------
        case(
            "nif_so/non_string_path_is_a_catchable_badarg",
            "a non-string path to load_nif/2 is a catchable badarg, while a binary path reaches the loader",
            &[15],
            // 1 (integer path refused) + 2 (tuple path refused) + 10 (binary path -> load_failed).
            "13",
            // No fixture: no library is loaded, so this case runs without a C toolchain.
            None,
        ),
    ]
}#[cfg(test)]
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
        // on a machine with no C toolchain. Every case that builds a library — the real one, the
        // non-NIF `.so`, and the veto / name / entry-header refusals — needs the C toolchain;
        // 5 (no fixture) and 6 (a text file named `<name>.so`) do not.
        let mut without_cc = 0;
        for case in group() {
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: expected a module-run case", case.id);
            };
            let needs_cc = matches!(run.nif, Some(NifFixture::CompiledLibrary { .. }));
            let loads_a_compiled_library = !matches!(run.args[0], 5 | 6 | 15);            assert_eq!(
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
    }}
