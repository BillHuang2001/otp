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

//! The built-in conformance corpus: one small, fast case group per compatibility contract.
//!
//! A case carries a [`Golden`] expectation of the oracle's observable output, plus a
//! [`CaseKind`] that says how the two runtimes are driven. The golden makes the corpus
//! self-testing: the oracle must reproduce it (if it stops, that is corpus drift — a real
//! failure, not a skip). There are three kinds of case:
//!
//! * [`CaseKind::Eval`] — a plain Erlang **expression** plus a [`Probe`]; both runtimes get
//!   the *same* `-eval` argv. These exercise the compatibility surface an expression can
//!   reach (ETF, semantics, BIFs, `system_info`, CLI flags).
//! * [`CaseKind::EvalEnv`] — the same, but each runtime is additionally spawned with an
//!   explicit environment, so the `ERL_*FLAGS` merge (contract A.6) is pinnable against the
//!   oracle.
//! * [`CaseKind::Module`] — a tiny embedded Erlang module is compiled with the system
//!   `erlc` and one exported function is applied. The two runtimes get *different* argv
//!   (the oracle's `-pa` + `-eval` versus the Rust runtime's `run` action), and only
//!   stdout + exit status are compared (a runtime's boot banner on stderr is not a
//!   semantic difference). This is the *agreement* evidence: a compiled `.beam` executed
//!   by the Rust runtime reproduces the oracle's value.
//!
//! Keep the default corpus **small and fast** (smoke level, Part I §C): heavy suites are
//! milestone-only. Cases are grouped by the contract they exercise:
//!
//! | module | contract |
//! |---|---|
//! | [`terms`] | A.1 — language semantics (ordering, equality) |
//! | [`semantics`] | A.1 — arithmetic, control flow, data structures, funs |
//! | [`errors`] | A.1 — the exception classes (`error`/`exit`/`throw`) and reasons |
//! | [`etf`] | A.2 — `.beam` binary / `term_to_binary` bytes |
//! | [`nif`] | A.3 — the observable `erlang:nif_error/1` NIF surface |
//! | [`nif_so`] | A.3 — the NIF C ABI *end to end*: a real `.so`-backed NIF round trip |
//! | [`driver`] | A.4 — the observable port/`spawn`-driver surface |
//! | [`driver_ports`] | A.4 — the `open_port/2` port-open error shapes + the missing-`spawn_executable` `enoent` shape |
//! | [`wire`] | A.5 — the distribution wire codec (ETF cross-encoding) |
//! | [`wire_ids`] | A.2/A.5 — `term_to_binary` of a local pid / port id, plus the A.5 node-identity surface (`node/0,1`, `nodes/0,1`, the `set_cookie/2` family) |
//! | [`flags`] | A.6 — CLI / configuration (incl. the `+hmax`/`+hmaxk`/`+hmaxel`/`+hmaxib`/`+hms`/`+hmbs` max-heap family) |
//! | [`bifs`] | A.7 — BIF / OTP app surface (incl. the timer, process-control and code-loading families, `erts_debug:size/1`, `erts_internal:cmp_term/2`) |
//! | [`phash`] | A.7 — `phash/2` and `phash2/1,2` (the externally-observable legacy hashes) |
//! | [`binmatch`] | A.7 — `binary:compile_pattern/1` + the compiled match/matches/split family |
//! | [`math_bifs`] | A.7 — the `math` module: bit-exact values + the `badarith` failure surface |
//! | [`printing`] | A.7 — the `~p`/`~W` printer and `short` float formatting |
//! | [`ets`] | A.7 — ETS tables: the `ets:*` BIF surface the runtime serves |
//! | [`ets_access`] | A.7 — the ETS access-control matrix (`public`/`protected`/`private` × owner/non-owner) |
//! | [`ets_spec`] | A.7 — the `ets` match-spec / `select` family + the `info/1,2` fields + the `ordered_set` arithmetic-key rule |
//! | [`proc_signals`] | A.7 — the group leader, the `dt_*` switches, `erlang:statistics/1`, `spawn_monitor`/`monitor` |
//! | [`proc_control`] | A.7 — `monitor`/`demonitor`, the `exit/1,2` taxonomy, `register`/`unregister`/`whereis`, the run-queue priorities |
//! | [`system_info`] | A.8 — introspection (incl. `erlang:memory/0,1` — a Rust capability gap — and the trace / `seq_trace` / `halt` surface) |
//! | [`boot`] | A.9 — boot/version observability (asserted by shape) |
//! | [`boot_prereq`] | A.9 — the boot-path prerequisite primitives (preloaded set, boot-script applications, `register`/`whereis`, group-leader inheritance, the `-boot` flag, the `proc_lib` blocked-seam gap) |
//! | [`modules`] | A.2/A.1/A.7 — a freshly `erlc`-compiled module run under both runtimes |
//! | [`mexec`] | A.2/A.7 — a *real* system stdlib/kernel module executed end to end |
//! | [`records`] | A.1/A.2/A.7 — native records: the `records:*` BIFs + the native-record opcodes |
//!
//! A.3 is reachable **end to end**, not only through its raise surface: the [`nif_so`] group
//! runs a compiled module that `erlang:load_nif/2`s a **real `.so`** (built into the case's
//! temp dir by [`crate::module_build::build`]) and calls the library's functions, so the NIF
//! *call* path and the `{error, {Reason, Text}}` failure shapes are exercised, not just
//! `erlang:nif_error/1`. The C-declaration half of A.3 and *all* of A.4 stay asserted
//! **structurally** in `tests/abi.rs` (a C translation unit that references the `enif_*` /
//! `driver_*` / `erl_drv_*` declarations must still compile) because the driver C ABI is not
//! observable through the black-box CLI at all.

mod bifs;
mod binmatch;
mod boot;
mod boot_extra;
mod boot_prereq;
mod driver;
mod driver_c;
mod driver_abi;
mod driver_abi_extra;
mod driver_ports;
mod errors;
mod etf;
mod ets;
mod ets_access;
mod ets_spec;
mod flags;
mod math_bifs;
mod mexec;
mod mexec_extra;
mod modules;
mod nif;
mod nif_abi;
mod nif_abi_extra;
mod nif_res;
mod nif_so;
mod phash;
mod printing;
mod proc_control;
mod proc_signals;
mod records;
mod semantics;
mod system_info;
mod terms;
mod wire;
mod wire_extra;
mod wire_ids;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::module_build::NifFixture;
use crate::runtime::eval_argv_owned;

/// One of the nine ERTS compatibility contracts that the rewrite must reproduce exactly
/// (`ERTS_RUST_REWRITE_EVALUATION.md` §A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Contract {
    /// A.1 — language semantics (term ordering/equality, arithmetic, exceptions).
    LanguageSemantics,
    /// A.2 — `.beam` binary / assembly and the canonical External Term Format bytes.
    BeamAsm,
    /// A.3 — the NIF C ABI (`enif_*`).
    NifAbi,
    /// A.4 — the driver C ABI (`driver_*` / `erl_drv_*`).
    DriverAbi,
    /// A.5 — distribution: wire protocol / handshake.
    Distribution,
    /// A.6 — CLI flags and configuration.
    Cli,
    /// A.7 — OTP application contracts (preloaded/boot, `erts_internal`, `prim_*`, BIFs).
    OtpApps,
    /// A.8 — introspection and tooling (`erlang:system_info/1`).
    Introspection,
    /// A.9 — build / bootstrapping.
    Build,
}

impl Contract {
    /// The stable contract id (`"A.1"` … `"A.9"`).
    pub fn id(&self) -> &'static str {
        match self {
            Contract::LanguageSemantics => "A.1",
            Contract::BeamAsm => "A.2",
            Contract::NifAbi => "A.3",
            Contract::DriverAbi => "A.4",
            Contract::Distribution => "A.5",
            Contract::Cli => "A.6",
            Contract::OtpApps => "A.7",
            Contract::Introspection => "A.8",
            Contract::Build => "A.9",
        }
    }

    /// A one-line human label for the contract.
    pub fn label(&self) -> &'static str {
        match self {
            Contract::LanguageSemantics => "language semantics",
            Contract::BeamAsm => ".beam binary/ETF bytes",
            Contract::NifAbi => "NIF C ABI",
            Contract::DriverAbi => "driver C ABI",
            Contract::Distribution => "distribution wire protocol",
            Contract::Cli => "CLI flags and configuration",
            Contract::OtpApps => "OTP application contracts",
            Contract::Introspection => "introspection / system_info",
            Contract::Build => "build and bootstrapping",
        }
    }
}

impl serde::Serialize for Contract {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.id())
    }
}

/// How a case's expression is wrapped before it is handed to a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Probe {
    /// Print `term_to_binary/1` of the result — the default (contract A.2).
    TermToBinary,
    /// Print the result with `~w` (textual rendering, no ETF encode).
    Value,
}

impl Probe {
    /// Stable identifier used in listings (`"term_to_binary"` / `"value"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Probe::TermToBinary => "term_to_binary",
            Probe::Value => "value",
        }
    }
}

/// The independent expectation a case holds about the oracle's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Golden {
    /// The expected stdout **line**, without the trailing newline that a runtime's
    /// `~n` (or the `run` action's printer) emits (`None` asserts nothing about stdout).
    pub stdout: Option<&'static str>,
    /// The expected process exit code (`0` for a value, `1` for a BEAM exception).
    pub exit_code: i32,
}

/// How a case drives the two runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseKind {
    /// An expression case: both runtimes evaluate the same self-terminating probe via
    /// `-eval`, so they receive **identical** argv (only the program path differs).
    Eval {
        /// The plain Erlang expression to evaluate.
        expr: &'static str,
        /// Extra argv inserted before `-eval` (e.g. `-<flag> <value>` for A.6 cases).
        extra_argv: &'static [&'static str],
        /// How the expression is wrapped into a self-terminating probe.
        probe: Probe,
    },
    /// An **environment-variable** expression case (contract A.6): exactly like
    /// [`CaseKind::Eval`] (identical `-eval` argv for both runtimes), except each runtime
    /// is additionally spawned with an explicit environment, so the `ERL_*FLAGS` merge —
    /// `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]` — can
    /// be pinned against the oracle end to end.
    ///
    /// It is a separate variant (rather than an `env` field on [`CaseKind::Eval`]) so the
    /// hundreds of existing expression cases stay untouched; every case that does not pin
    /// the merge reports an empty [`Case::env`].
    EvalEnv {
        /// The plain Erlang expression to evaluate.
        expr: &'static str,
        /// Extra argv inserted before `-eval` — the "command line" of the A.6 merge, so it
        /// sits *after* `ERL_AFLAGS` and *before* `ERL_FLAGS`/`ERL_ZFLAGS`.
        extra_argv: &'static [&'static str],
        /// The `(name, value)` variables the runtime is spawned with, set **on top of** the
        /// inherited environment (which is never cleared).
        env: &'static [(&'static str, &'static str)],
        /// How the expression is wrapped into a self-terminating probe.
        probe: Probe,
    },
    /// A module-run case: `erlc` compiles an embedded module and one exported function is
    /// applied. The two runtimes receive **different** argv — the oracle's
    /// `-noshell -pa <dir> -eval …` versus the Rust runtime's
    /// `run <dir>/<module>.beam <module>:<fun>/<arity> <args…>`.
    Module(ModuleRun),
}

/// One embedded-module function invocation (the data of a [`CaseKind::Module`] case).
///
/// The source is compiled at run time by [`crate::module_build`] with the system `erlc`;
/// string and slice fields are `'static` so a [`Case`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleRun {
    /// The embedded Erlang source, compiled at run time with the system `erlc`.
    pub source: &'static str,
    /// The module name (must match the `-module` attribute in `source`).
    pub module: &'static str,
    /// The exported function applied by both runtimes.
    pub function: &'static str,
    /// The function arity (equal to `args.len()`).
    pub arity: u32,
    /// The integer arguments passed to the function.
    pub args: &'static [i64],
    /// The C/NIF artifacts the module loads at run time (contract A.3), materialized into the
    /// **same** temp dir as the compiled `.beam` — the module locates them with
    /// `filename:dirname(code:which(?MODULE))`, so neither runtime's argv changes.
    ///
    /// `None` for the ordinary module-run cases (and for a NIF case that deliberately loads
    /// a path naming no library at all, which needs no artifact and therefore no compiler).
    pub nif: Option<NifFixture>,
}

impl ModuleRun {
    /// The `<dir>/<module>.beam` path inside a compiled module's temporary directory.
    pub fn beam_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.beam", self.module))
    }

    /// The oracle's `-eval` expression: apply the function and print the result with `~w`.
    ///
    /// `beam_rt run` prints the value the same way (with its own trailing newline), so
    /// this single line is the observable both runtimes must reproduce.
    pub fn oracle_eval(&self) -> String {
        let args: Vec<String> = self.args.iter().map(i64::to_string).collect();
        format!(
            "io:format(\"~w~n\",[{0}:{1}({2})]), halt(0).",
            self.module,
            self.function,
            args.join(",")
        )
    }

    /// The oracle argv: `-noshell -pa <dir> -eval <oracle_eval>`.
    pub fn oracle_argv(&self, dir: &Path) -> Vec<String> {
        eval_argv_owned(
            &self.oracle_eval(),
            &["-pa".to_string(), dir.display().to_string()],
        )
    }

    /// The Rust runtime argv: `run <dir>/<module>.beam <module>:<fun>/<arity> <args…>`.
    pub fn rust_argv(&self, dir: &Path) -> Vec<String> {
        let mut argv = vec![
            "run".to_string(),
            self.beam_path(dir).display().to_string(),
            format!("{}:{}/{}", self.module, self.function, self.arity),
        ];
        argv.extend(self.args.iter().map(i64::to_string));
        argv
    }
}

/// One conformance case: how to drive the runtimes and what the oracle must print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Case {
    /// Stable case id, `<group>/<name>` (unique across the corpus).
    pub id: &'static str,
    /// One-line description of the behaviour being checked.
    pub description: &'static str,
    /// The compatibility contracts this case covers (at least one).
    pub contracts: &'static [Contract],
    /// How the two runtimes are driven (see [`CaseKind`]).
    pub kind: CaseKind,
    /// What the oracle must print for this case.
    pub golden: Golden,
}

impl Case {
    /// The full probe expression handed to both runtimes for this case.
    ///
    /// For a [`CaseKind::Eval`] case this is the self-terminating probe wrapper; for a
    /// [`CaseKind::Module`] case it is the oracle's `io:format` expression (the Rust
    /// runtime is invoked through its `run` action instead — see [`CaseKind::Module`]).
    pub fn eval(&self) -> String {
        match &self.kind {
            CaseKind::Eval { expr, probe, .. } | CaseKind::EvalEnv { expr, probe, .. } => {
                probe_expr(expr, *probe)
            }
            CaseKind::Module(run) => run.oracle_eval(),
        }
    }

    /// The stable kind label used in listings: the probe name for eval cases, and
    /// `"module-run"` for module cases.
    pub fn kind_label(&self) -> &'static str {
        match &self.kind {
            CaseKind::Eval { probe, .. } | CaseKind::EvalEnv { probe, .. } => probe.as_str(),
            CaseKind::Module(_) => "module-run",
        }
    }

    /// The plain Erlang expression (`None` for a module-run case).
    pub fn expr(&self) -> Option<&'static str> {
        match &self.kind {
            CaseKind::Eval { expr, .. } | CaseKind::EvalEnv { expr, .. } => Some(expr),
            CaseKind::Module(_) => None,
        }
    }

    /// The extra eval argv (`&[]` for a module-run case).
    pub fn extra_argv(&self) -> &'static [&'static str] {
        match &self.kind {
            CaseKind::Eval { extra_argv, .. } | CaseKind::EvalEnv { extra_argv, .. } => extra_argv,
            CaseKind::Module(_) => &[],
        }
    }

    /// The explicit environment the runtimes are spawned with (`&[]` for every case that
    /// does not pin the `ERL_*FLAGS` merge — see [`CaseKind::EvalEnv`]).
    pub fn env(&self) -> &'static [(&'static str, &'static str)] {
        match &self.kind {
            CaseKind::EvalEnv { env, .. } => env,
            CaseKind::Eval { .. } | CaseKind::Module(_) => &[],
        }
    }

    /// The contract ids of this case, in declaration order.
    pub fn contract_ids(&self) -> Vec<&'static str> {
        self.contracts.iter().map(Contract::id).collect()
    }
}

static CASES: OnceLock<Vec<Case>> = OnceLock::new();

fn build_corpus() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(etf::group());
    cases.extend(terms::group());
    cases.extend(semantics::group());
    cases.extend(errors::group());
    cases.extend(nif::group());
    cases.extend(nif_so::group());
    cases.extend(nif_res::group());
    cases.extend(nif_abi::group());
    cases.extend(nif_abi_extra::group());
    cases.extend(records::group());
    cases.extend(driver::group());
    cases.extend(driver_c::group());
    cases.extend(driver_abi::group());
    cases.extend(driver_abi_extra::group());
    cases.extend(driver_ports::group());
    cases.extend(wire::group());
    cases.extend(wire_ids::group());
    cases.extend(wire_extra::group());
    cases.extend(bifs::group());
    cases.extend(phash::group());
    cases.extend(binmatch::group());
    cases.extend(math_bifs::group());
    cases.extend(printing::group());
    cases.extend(ets::group());
    cases.extend(ets_access::group());
    cases.extend(ets_spec::group());
    cases.extend(proc_signals::group());
    cases.extend(proc_control::group());
    cases.extend(system_info::group());
    cases.extend(boot::group());
    cases.extend(boot_prereq::group());
    cases.extend(boot_extra::group());
    cases.extend(flags::group());
    cases.extend(mexec::group());
    cases.extend(mexec_extra::group());
    cases.extend(modules::group());
    cases
}

/// The whole built-in corpus (built once, in a stable order).
pub fn cases() -> &'static [Case] {
    CASES.get_or_init(build_corpus).as_slice()
}

/// Look a case up by its id.
pub fn find_case(id: &str) -> Option<&'static Case> {
    cases().iter().find(|case| case.id == id)
}

/// Wrap a plain expression into the self-terminating probe both runtimes are given.
///
/// The wrapper is mandatory: `erl -noshell -eval '<expr with an uncaught error>'` prints
/// *nothing* and exits 1, so without it a BEAM exception would be invisible to a stdout
/// diff. Both probes print exactly one line and then halt with the corresponding code:
/// `ok <repr>` / exit `0`, or `exception <class> <reason>` / exit `1`.
pub fn probe_expr(expr: &str, probe: Probe) -> String {
    match probe {
        Probe::TermToBinary => format!(
            "try ({expr}) of V -> io:format(\"ok ~w~n\", [term_to_binary(V)]), halt(0) \
             catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
        ),
        Probe::Value => format!(
            "try ({expr}) of V -> io:format(\"ok ~w~n\", [V]), halt(0) \
             catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{cases, find_case, probe_expr, CaseKind, Contract, Probe};
    use std::collections::HashSet;

    #[test]
    fn case_ids_are_unique_and_well_formed() {
        let mut seen = HashSet::new();
        for case in cases() {
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
            assert!(
                case.id.contains('/'),
                "case id {} is not <group>/<name>",
                case.id
            );
            assert!(!case.id.starts_with('/') && !case.id.ends_with('/'));
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
        }
    }

    #[test]
    fn every_case_covers_at_least_one_contract_and_every_group_is_represented() {
        let mut contracts = HashSet::new();
        for case in cases() {
            assert!(
                !case.contracts.is_empty(),
                "case {} covers no contract",
                case.id
            );
            for contract in case.contracts {
                contracts.insert(contract.id());
            }
        }
        // The corpus must exercise every one of the nine contracts (A.1–A.9); A.4 is
        // observed through the port/spawn-driver surface, the rest through their groups.
        for expected in ["A.1", "A.2", "A.3", "A.4", "A.5", "A.6", "A.7", "A.8", "A.9"] {
            assert!(contracts.contains(expected), "no case covers {expected}");
        }
        assert_eq!(
            Contract::Introspection.id(),
            "A.8",
            "ids are part of the report schema"
        );
        assert!(!Contract::BeamAsm.label().is_empty());
    }

    #[test]
    fn environment_cases_are_well_formed_and_never_carry_an_empty_env() {
        let mut env_cases = 0;
        for case in cases() {
            match &case.kind {
                CaseKind::EvalEnv { env, .. } => {
                    env_cases += 1;
                    assert!(
                        !env.is_empty(),
                        "case {}: an environment case must set at least one variable",
                        case.id
                    );
                    assert_eq!(
                        case.env(),
                        *env,
                        "case {}: the env accessor agrees",
                        case.id
                    );
                }
                CaseKind::Eval { .. } | CaseKind::Module(_) => {
                    assert!(
                        case.env().is_empty(),
                        "case {}: only an EvalEnv case may set an environment",
                        case.id
                    );
                }
            }
        }
        assert!(
            env_cases > 0,
            "the corpus must carry environment-variable (`ERL_*FLAGS`) cases"
        );
    }

    #[test]
    fn every_case_has_a_non_empty_golden_stdout() {
        for case in cases() {
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {} has no golden stdout", case.id));
            assert!(!golden.is_empty(), "case {} has an empty golden", case.id);
            assert!(
                !golden.ends_with('\n'),
                "case {}: the golden is a line, without the trailing newline",
                case.id
            );
            match &case.kind {
                CaseKind::Eval { expr, .. } | CaseKind::EvalEnv { expr, .. } => {
                    assert!(
                        golden.starts_with("ok ") || golden.starts_with("exception "),
                        "case {}: unexpected golden shape {golden:?}",
                        case.id
                    );
                    assert!(
                        !expr.is_empty() && !expr.contains('\n'),
                        "case {} has a suspicious expression",
                        case.id
                    );
                    if golden.starts_with("exception ") {
                        assert_ne!(
                            case.golden.exit_code, 0,
                            "case {}: an exception must not exit 0",
                            case.id
                        );
                    } else {
                        assert_eq!(
                            case.golden.exit_code, 0,
                            "case {}: a successful probe exits 0",
                            case.id
                        );
                    }
                }
                CaseKind::Module(run) => {
                    assert!(
                        !run.source.is_empty() && run.source.contains("-module("),
                        "case {}: suspicious module source",
                        case.id
                    );
                    assert!(
                        golden.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()),
                        "case {}: a module-run golden is a plain integer line, got {golden:?}",
                        case.id
                    );
                    assert_eq!(
                        case.golden.exit_code, 0,
                        "case {}: a successful module run exits 0",
                        case.id
                    );
                    assert_eq!(
                        run.arity as usize,
                        run.args.len(),
                        "case {}: arity must match the argument count",
                        case.id
                    );
                }
            }
        }
    }

    #[test]
    fn find_case_looks_cases_up_by_id() {
        let case = find_case("etf/term_to_binary_tuple").expect("case present");
        assert_eq!(case.contracts, &[Contract::BeamAsm]);
        assert!(matches!(
            case.kind,
            CaseKind::Eval {
                probe: Probe::TermToBinary,
                ..
            }
        ));
        assert_eq!(case.golden.exit_code, 0);
        assert_eq!(case.kind_label(), "term_to_binary");
        assert_eq!(case.expr(), Some("{ok,42}"));
        assert!(find_case("nope/nope").is_none());
    }

    #[test]
    fn module_run_cases_are_registered_and_well_formed() {
        let mut module_cases = 0;
        for case in cases() {
            let CaseKind::Module(run) = &case.kind else {
                continue;
            };
            module_cases += 1;
            assert_eq!(case.kind_label(), "module-run");
            assert_eq!(case.expr(), None);
            assert!(case.extra_argv().is_empty());
            assert!(
                case.contracts.contains(&Contract::BeamAsm),
                "case {}: a module-run case must evidence the .beam contract",
                case.id
            );
            assert!(
                case.contracts
                    .iter()
                    .all(|c| matches!(
                        c,
                        Contract::BeamAsm
                            | Contract::LanguageSemantics
                            | Contract::OtpApps
                            // `nif_so` (A.3): a module-run case that loads a real NIF `.so`
                            // evidences the NIF C ABI end to end — the library is compiled
                            // from C against the host `erl_nif.h` and its functions run
                            // under both runtimes (see `super::nif_so`).
                            | Contract::NifAbi
                            // `driver_c` (A.4): a module-run case that opens a **real C
                            // driver** `.so` compiled against the host `erl_driver.h` and
                            // drives it through the port lifecycle (`start` → `output` →
                            // `stop`) evidences the driver C ABI end to end
                            // (see `super::driver_c`).
                            | Contract::DriverAbi
                    )),
                "case {}: a module-run case may only claim the contracts it evidences",
                case.id
            );
            assert!(
                run.oracle_eval().contains(&format!("{}:{}(", run.module, run.function)),
                "case {}: the oracle eval must apply the function",
                case.id
            );
            assert_eq!(
                run.oracle_argv(std::path::Path::new("/code")),
                vec![
                    "-noshell".to_string(),
                    "-pa".to_string(),
                    "/code".to_string(),
                    "-eval".to_string(),
                    run.oracle_eval(),
                ],
                "case {}: the oracle argv is the drop-in erl shape",
                case.id
            );
            let rust_argv = run.rust_argv(std::path::Path::new("/code"));
            assert_eq!(
                rust_argv[..3],
                [
                    "run".to_string(),
                    format!("/code/{}.beam", run.module),
                    format!("{}:{}/{}", run.module, run.function, run.arity),
                ],
                "case {}: the Rust argv uses the run action",
                case.id
            );
            assert_eq!(
                &rust_argv[3..],
                run.args
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .as_slice(),
                "case {}: the integer arguments follow the spec",
                case.id
            );
        }
        assert!(
            module_cases >= 20,
            "the corpus must carry a broad set of module-run agreement cases (got {module_cases})"
        );
    }

    #[test]
    fn probe_expr_wraps_the_expression_and_self_terminates() {
        let t2b = probe_expr("{ok,42}", Probe::TermToBinary);
        assert!(t2b.starts_with("try ({ok,42}) of V -> io:format(\"ok ~w~n\", [term_to_binary(V)]), halt(0)"));
        assert!(t2b.ends_with("halt(1) end."));
        assert!(t2b.contains("catch C:R -> io:format(\"exception ~w ~w~n\", [C, R])"));

        let value = probe_expr("1+1", Probe::Value);
        assert!(value.contains("io:format(\"ok ~w~n\", [V])"));
        assert!(!value.contains("term_to_binary"));
        assert_eq!(Probe::Value.as_str(), "value");
    }
}
