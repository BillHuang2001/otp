//! Contract **A.2** — `.beam` binary: a freshly `erlc`-compiled module run under both runtimes.
//!
//! These are the corpus's **agreement** evidence. Unlike the expression groups (which the
//! Rust runtime currently reports `unsupported`, a capability gap), a compiled `.beam` is
//! within the Rust runtime's `run` subset, so the two runtimes actually produce — and are
//! compared on — the same observable value.
//!
//! The module source is embedded here and compiled at run time with the system `erlc`
//! (see [`crate::module_build`]); the harness invokes the oracle through
//! `-noshell -pa <dir> -eval …` and the Rust runtime through `run <dir>/<mod>.beam …`.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The source compiled for every module-run case.
///
/// It is deliberately tiny and uses only arithmetic and self-recursion, so it lowers to
/// the opcodes the Rust runtime already supports — the point is to compare a *real*
/// compiled `.beam`, not to stress a feature gap.
pub(crate) const DIFF_MOD_SOURCE: &str = "-module(diff_mod).\n\
     -export([add/2, fib/1]).\n\
     add(A,B) -> A+B.\n\
     fib(N) -> fib(N,0,1).\n\
     fib(0,A,_B) -> A;\n\
     fib(N,A,B) -> fib(N-1,B,A+B).\n";

/// The A.2 module-run cases: `add/2` and `fib/1` must agree between the two runtimes.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "modules/add",
            description: "a freshly erlc-compiled module's add/2 returns the same value under both runtimes",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Module(ModuleRun {
                source: DIFF_MOD_SOURCE,
                module: "diff_mod",
                function: "add",
                arity: 2,
                args: &[2, 3],
            }),
            golden: Golden {
                stdout: Some("5"),
                exit_code: 0,
            },
        },
        Case {
            id: "modules/fib",
            description: "a freshly erlc-compiled module's fib/1 returns the same value under both runtimes",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Module(ModuleRun {
                source: DIFF_MOD_SOURCE,
                module: "diff_mod",
                function: "fib",
                arity: 1,
                args: &[10],
            }),
            golden: Golden {
                stdout: Some("55"),
                exit_code: 0,
            },
        },
    ]
}
