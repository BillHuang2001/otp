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

//! Command line: hand-rolled, `std`-only argument parsing (no `clap`).
//!
//! The grammar is deliberately subcommand-first:
//!
//! ```text
//! differential <SUBCOMMAND> [OPTIONS]
//!   run     <case-id> [--json] [--out <dir>]
//!   compare <expr> [--pa <dir>]… [--json] [--out <dir>]
//!   suite   [--strict] [--allowlist <file>] [--gaps <path>] [--json] [--out <dir>]
//!   list    [--by-contract] [--json] [--out <dir>]
//!   help    [topic]
//! ```
//!
//! `-h`/`--help` is recognized **anywhere** — as the first argument (root help) or after
//! any subcommand (that subcommand's help), which is what [`Command::Help`]'s `topic`
//! records. Every failure to understand the command line is an [`Error::Usage`] (exit 2).

use std::path::PathBuf;

use crate::error::{Error, Result};

/// A fully parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run one built-in corpus case under both runtimes and compare the observations.
    Run {
        /// The corpus case id (see `differential list`).
        case_id: String,
        /// Print the JSON report to stdout.
        json: bool,
        /// Also write `<dir>/report.json`.
        out: Option<PathBuf>,
    },
    /// Evaluate an ad-hoc Erlang expression under both runtimes and compare.
    Compare {
        /// The Erlang expression to evaluate under the default probe.
        expr: String,
        /// Code paths passed to both runtimes as `-pa <dir>` (repeatable).
        pa: Vec<String>,
        /// Print the JSON report to stdout.
        json: bool,
        /// Also write `<dir>/report.json`.
        out: Option<PathBuf>,
    },
    /// Run the whole built-in conformance corpus.
    Suite {
        /// Escalate `unsupported`/`skipped` cases to a failure exit code.
        strict: bool,
        /// A known-gaps allowlist: tolerates the gaps it records, fails on an unauthorized
        /// gap or a stale entry. Requires `strict`.
        allowlist: Option<PathBuf>,
        /// Also write the generated `GAPS.md` document to this path.
        gaps: Option<PathBuf>,
        /// Print the JSON report to stdout.
        json: bool,
        /// Also write `<dir>/report.json`.
        out: Option<PathBuf>,
    },
    /// List the built-in corpus (needs no runtime at all).
    List {
        /// Group the listing by compatibility contract (`A.1` … `A.9`).
        by_contract: bool,
        /// Print the JSON listing to stdout.
        json: bool,
        /// Also write `<dir>/report.json`.
        out: Option<PathBuf>,
    },
    /// Print help for the root (`topic == None`) or for a subcommand.
    Help {
        /// The subcommand whose help was requested.
        topic: Option<String>,
    },
}

/// Parse the harness arguments (**without** the program name, i.e. `argv[1..]`).
pub fn parse(args: &[String]) -> Result<Command> {
    let Some(first) = args.first() else {
        return Err(Error::usage(format!(
            "missing subcommand\n\n{}",
            usage(None)
        )));
    };
    if is_help(first) {
        return Ok(Command::Help { topic: None });
    }
    let rest = &args[1..];
    match first.as_str() {
        "run" => parse_run(rest),
        "compare" => parse_compare(rest),
        "suite" => parse_suite(rest),
        "list" => parse_list(rest),
        "help" => Ok(Command::Help {
            topic: rest.first().map(|topic| topic.to_string()),
        }),
        other => Err(Error::usage(format!(
            "unknown subcommand '{other}'\n\n{}",
            usage(None)
        ))),
    }
}

/// The human-readable usage text for the root (`topic == None`) or a subcommand.
pub fn usage(topic: Option<&str>) -> String {
    match topic {
        None | Some("help") => ROOT_USAGE.to_string(),
        Some("run") => format!(
            "differential run <case-id> [--json] [--out <dir>]\n\n\
             Run one built-in corpus case under the oracle and the Rust runtime, then\n\
             compare the observations (an expression case with one shared argv; a\n\
             module-run case with per-side argv) plus the case golden.\n\
             An unknown <case-id> is a usage error (exit 2).\n\n\
             OPTIONS:\n  \
             --json        print the JSON report to stdout\n  \
             --out <dir>   also write <dir>/report.json\n  \
             -h, --help    show this help\n\n\
             See `differential list` for the available case ids.\n"
        ),
        Some("compare") => format!(
            "differential compare <expr> [--pa <dir>]... [--json] [--out <dir>]\n\n\
             Evaluate an ad-hoc Erlang expression under both runtimes using the default\n\
             `term_to_binary/1` probe, then compare the observations. No golden is\n\
             asserted (an ad-hoc expression has none).\n\n\
             OPTIONS:\n  \
             --pa <dir>    add a code path for both runtimes (repeatable)\n  \
             --json        print the JSON report to stdout\n  \
             --out <dir>   also write <dir>/report.json\n  \
             -h, --help    show this help\n"
        ),
        Some("suite") => format!(
            "differential suite [--strict] [--allowlist <file>] [--gaps <path>] [--json] [--out <dir>]\n\n\
             Run the whole built-in conformance corpus and compare every case.\n\n\
             OPTIONS:\n  \
             --strict            treat `unsupported`/`skipped` cases as failures (exit 1)\n  \
             --allowlist <file>  ratchet: tolerate the known capability gaps recorded in <file>\n                      (requires --strict); fail on an unauthorized gap or a stale entry\n  \
             --gaps <path>       also write the generated GAPS.md document to <path>\n  \
             --json              print the JSON report to stdout\n  \
             --out <dir>         also write <dir>/report.json\n  \
             -h, --help          show this help\n"
        ),
        Some("list") => format!(
            "differential list [--by-contract] [--json] [--out <dir>]\n\n\
             List the built-in corpus (case id, contracts, description). Needs no runtime\n\
             and always exits 0.\n\n\
             OPTIONS:\n  \
             --by-contract group the listing by compatibility contract (A.1...A.9)\n  \
             --json        print the JSON listing to stdout\n  \
             --out <dir>   also write <dir>/report.json\n  \
             -h, --help    show this help\n"
        ),
        Some(other) => format!(
            "unknown help topic '{other}'\n\n{}",
            usage(None)
        ),
    }
}

/// The root usage text: subcommands, options, environment and exit codes.
pub const ROOT_USAGE: &str = "\
differential — Rust-ERTS vs system-ERTS differential-test harness

USAGE:
  differential <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS:
  run <case-id>   run one built-in corpus case under both runtimes and compare
  compare <expr>  evaluate an ad-hoc Erlang expression under both runtimes
  suite           run the whole built-in conformance corpus
  list            list the built-in corpus (needs no runtime)
  help [topic]    show this help, or the help of one subcommand

OPTIONS (per subcommand):
  --json          print the JSON report to stdout
  --out <dir>     also write <dir>/report.json
  --strict        (suite) treat unsupported/skipped cases as failures
  --allowlist <file> (suite, with --strict) tolerate the recorded known capability gaps
  --gaps <path>   (suite) also write the generated GAPS.md document to <path>
  --by-contract   (list) group the listing by compatibility contract
  --pa <dir>      (compare) add a code path for both runtimes, repeatable
  -h, --help      show help (valid at the root and after any subcommand)

ENVIRONMENT:
  ERTS_ORACLE     oracle runtime; defaults to `erl` resolved on $PATH
  BEAM_RT         Rust runtime; defaults to the workspace-relative
                  erts_rust/target/{release,debug}/beam_rt
  ERLC            module compiler for the module-run cases; defaults to `erlc`
                  resolved on $PATH (a missing `erlc` skips those cases)
EXIT CODES:
  0  every executed case agreed (unsupported/skipped are reported, not fatal)
  1  at least one mismatch (including oracle-vs-its-own-golden corpus drift)
  2  usage, environment or IO error
";

fn is_help(arg: &str) -> bool {
    arg == "-h" || arg == "--help"
}

/// Handle the shared `--json` / `--out <dir>` options.
///
/// Returns `true` when the option was consumed.
fn consume_common(
    arg: &str,
    args: &[String],
    index: &mut usize,
    json: &mut bool,
    out: &mut Option<PathBuf>,
    subcommand: &str,
) -> Result<bool> {
    if arg == "--json" {
        *json = true;
        return Ok(true);
    }
    if arg == "--out" {
        *index += 1;
        let value = args.get(*index).ok_or_else(|| {
            Error::usage(format!(
                "option '--out' requires a directory argument\n\n{}",
                usage(Some(subcommand))
            ))
        })?;
        *out = Some(PathBuf::from(value));
        return Ok(true);
    }
    if let Some(value) = arg.strip_prefix("--out=") {
        if value.is_empty() {
            return Err(Error::usage(format!(
                "option '--out' requires a directory argument\n\n{}",
                usage(Some(subcommand))
            )));
        }
        *out = Some(PathBuf::from(value));
        return Ok(true);
    }
    Ok(false)
}

fn unknown_option(arg: &str, subcommand: &str) -> Error {
    Error::usage(format!(
        "unknown option '{arg}' for '{subcommand}'\n\n{}",
        usage(Some(subcommand))
    ))
}

fn parse_run(args: &[String]) -> Result<Command> {
    let (mut json, mut out, mut case_id) = (false, None, None);
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if is_help(arg) {
            return Ok(Command::Help {
                topic: Some("run".to_string()),
            });
        }
        if consume_common(arg, args, &mut index, &mut json, &mut out, "run")? {
            index += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            return Err(unknown_option(arg, "run"));
        }
        if case_id.is_some() {
            return Err(Error::usage(format!(
                "'run' accepts exactly one <case-id>\n\n{}",
                usage(Some("run"))
            )));
        }
        case_id = Some(arg.clone());
        index += 1;
    }
    let case_id = case_id.ok_or_else(|| {
        Error::usage(format!("'run' requires a <case-id>\n\n{}", usage(Some("run"))))
    })?;
    Ok(Command::Run { case_id, json, out })
}

fn parse_compare(args: &[String]) -> Result<Command> {
    let (mut json, mut out, mut expr, mut pa) = (false, None, None, Vec::new());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if is_help(arg) {
            return Ok(Command::Help {
                topic: Some("compare".to_string()),
            });
        }
        if consume_common(arg, args, &mut index, &mut json, &mut out, "compare")? {
            index += 1;
            continue;
        }
        if arg == "--pa" {
            index += 1;
            let value = args.get(index).ok_or_else(|| {
                Error::usage(format!(
                    "option '--pa' requires a directory argument\n\n{}",
                    usage(Some("compare"))
                ))
            })?;
            pa.push(value.clone());
            index += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--pa=") {
            if value.is_empty() {
                return Err(Error::usage(format!(
                    "option '--pa' requires a directory argument\n\n{}",
                    usage(Some("compare"))
                )));
            }
            pa.push(value.to_string());
            index += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            return Err(unknown_option(arg, "compare"));
        }
        if expr.is_some() {
            return Err(Error::usage(format!(
                "'compare' accepts exactly one <expr> (quote the expression)\n\n{}",
                usage(Some("compare"))
            )));
        }
        expr = Some(arg.clone());
        index += 1;
    }
    let expr = expr.ok_or_else(|| {
        Error::usage(format!(
            "'compare' requires an <expr>\n\n{}",
            usage(Some("compare"))
        ))
    })?;
    Ok(Command::Compare {
        expr,
        pa,
        json,
        out,
    })
}

fn parse_suite(args: &[String]) -> Result<Command> {
    let (mut json, mut out, mut strict) = (false, None, false);
    let mut allowlist: Option<PathBuf> = None;
    let mut gaps: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if is_help(arg) {
            return Ok(Command::Help {
                topic: Some("suite".to_string()),
            });
        }
        if arg == "--strict" {
            strict = true;
            index += 1;
            continue;
        }
        if arg == "--allowlist" {
            index += 1;
            let value = args.get(index).ok_or_else(|| {
                Error::usage(format!(
                    "option '--allowlist' requires a file argument\n\n{}",
                    usage(Some("suite"))
                ))
            })?;
            allowlist = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--allowlist=") {
            if value.is_empty() {
                return Err(Error::usage(format!(
                    "option '--allowlist' requires a file argument\n\n{}",
                    usage(Some("suite"))
                )));
            }
            allowlist = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if arg == "--gaps" {
            index += 1;
            let value = args.get(index).ok_or_else(|| {
                Error::usage(format!(
                    "option '--gaps' requires a path argument\n\n{}",
                    usage(Some("suite"))
                ))
            })?;
            gaps = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--gaps=") {
            if value.is_empty() {
                return Err(Error::usage(format!(
                    "option '--gaps' requires a path argument\n\n{}",
                    usage(Some("suite"))
                )));
            }
            gaps = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if consume_common(arg, args, &mut index, &mut json, &mut out, "suite")? {
            index += 1;
            continue;
        }
        return Err(unknown_option(arg, "suite"));
    }
    if allowlist.is_some() && !strict {
        return Err(Error::usage(format!(
            "option '--allowlist' requires '--strict'\n\n{}",
            usage(Some("suite"))
        )));
    }
    Ok(Command::Suite {
        strict,
        allowlist,
        gaps,
        json,
        out,
    })
}

fn parse_list(args: &[String]) -> Result<Command> {
    let (mut json, mut out, mut by_contract) = (false, None, false);
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if is_help(arg) {
            return Ok(Command::Help {
                topic: Some("list".to_string()),
            });
        }
        if arg == "--by-contract" {
            by_contract = true;
            index += 1;
            continue;
        }
        if consume_common(arg, args, &mut index, &mut json, &mut out, "list")? {
            index += 1;
            continue;
        }
        return Err(unknown_option(arg, "list"));
    }
    Ok(Command::List {
        by_contract,
        json,
        out,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, usage, Command, ROOT_USAGE};
    use crate::error::Error;
    use std::path::PathBuf;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn no_arguments_is_a_usage_error_with_help() {
        let err = parse(&[]).unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("missing subcommand"));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn root_help_is_recognized_in_both_spellings() {
        for spelling in ["-h", "--help"] {
            assert_eq!(
                parse(&argv(&[spelling])).unwrap(),
                Command::Help { topic: None }
            );
        }
    }

    #[test]
    fn help_is_recognized_after_any_subcommand() {
        for sub in ["run", "compare", "suite", "list"] {
            for spelling in ["-h", "--help"] {
                let parsed = parse(&argv(&[sub, spelling])).unwrap();
                assert_eq!(
                    parsed,
                    Command::Help {
                        topic: Some(sub.to_string())
                    }
                );
            }
        }
    }

    #[test]
    fn help_subcommand_takes_a_topic() {
        assert_eq!(
            parse(&argv(&["help"])).unwrap(),
            Command::Help { topic: None }
        );
        assert_eq!(
            parse(&argv(&["help", "suite"])).unwrap(),
            Command::Help {
                topic: Some("suite".to_string())
            }
        );
    }

    #[test]
    fn every_subcommand_has_usable_help() {
        assert!(usage(None).contains("SUBCOMMANDS"));
        assert_eq!(usage(None), ROOT_USAGE);
        for sub in ["run", "compare", "suite", "list", "help"] {
            let text = usage(Some(sub));
            assert!(text.contains(sub), "missing {sub} in its own help");
            assert!(text.contains("-h, --help"), "no help option for {sub}");
        }
        for sub in ["run", "compare", "suite", "list"] {
            let text = usage(Some(sub));
            assert!(text.contains("--out <dir>"), "{sub}: no --out documented");
            assert!(text.contains("USAGE") || text.starts_with("differential "), "{sub}");
        }
        assert!(usage(Some("nope")).contains("unknown help topic"));
    }

    #[test]
    fn parse_run_accepts_id_and_options_in_any_order() {
        assert_eq!(
            parse(&argv(&["run", "etf/term_to_binary_tuple"])).unwrap(),
            Command::Run {
                case_id: "etf/term_to_binary_tuple".to_string(),
                json: false,
                out: None,
            }
        );
        assert_eq!(
            parse(&argv(&[
                "run",
                "--json",
                "--out",
                "/tmp/x",
                "etf/term_to_binary_atom"
            ]))
            .unwrap(),
            Command::Run {
                case_id: "etf/term_to_binary_atom".to_string(),
                json: true,
                out: Some(PathBuf::from("/tmp/x")),
            }
        );
        assert_eq!(
            parse(&argv(&["run", "--out=/tmp/y", "terms/atom_order"])).unwrap(),
            Command::Run {
                case_id: "terms/atom_order".to_string(),
                json: false,
                out: Some(PathBuf::from("/tmp/y")),
            }
        );
    }

    #[test]
    fn parse_run_rejects_missing_id_and_extra_positionals() {
        assert!(matches!(parse(&argv(&["run"])), Err(Error::Usage(_))));
        assert!(matches!(parse(&argv(&["run", "--json"])), Err(Error::Usage(_))));
        assert!(matches!(
            parse(&argv(&["run", "a", "b"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn parse_compare_collects_pa_paths() {
        assert_eq!(
            parse(&argv(&[
                "compare",
                "length([a,b,c])",
                "--pa",
                "ebin",
                "--pa=/other",
                "--json"
            ]))
            .unwrap(),
            Command::Compare {
                expr: "length([a,b,c])".to_string(),
                pa: vec!["ebin".to_string(), "/other".to_string()],
                json: true,
                out: None,
            }
        );
    }

    #[test]
    fn parse_suite_handles_strict() {
        assert_eq!(
            parse(&argv(&["suite", "--strict", "--json"])).unwrap(),
            Command::Suite {
                strict: true,
                allowlist: None,
                gaps: None,
                json: true,
                out: None,
            }
        );
        assert_eq!(
            parse(&argv(&["suite"])).unwrap(),
            Command::Suite {
                strict: false,
                allowlist: None,
                gaps: None,
                json: false,
                out: None,
            }
        );
    }

    #[test]
    fn parse_suite_handles_the_allowlist_and_gaps_options() {
        // Both spellings of each option, in any order.
        assert_eq!(
            parse(&argv(&[
                "suite",
                "--strict",
                "--allowlist",
                "known-gaps.txt",
                "--gaps",
                "GAPS.md",
            ]))
            .unwrap(),
            Command::Suite {
                strict: true,
                allowlist: Some(PathBuf::from("known-gaps.txt")),
                gaps: Some(PathBuf::from("GAPS.md")),
                json: false,
                out: None,
            }
        );
        assert_eq!(
            parse(&argv(&[
                "suite",
                "--strict",
                "--gaps=out/GAPS.md",
                "--allowlist=known-gaps.txt",
            ]))
            .unwrap(),
            Command::Suite {
                strict: true,
                allowlist: Some(PathBuf::from("known-gaps.txt")),
                gaps: Some(PathBuf::from("out/GAPS.md")),
                json: false,
                out: None,
            }
        );
        // `--gaps` is independent of `--strict`; `--allowlist` is not.
        assert_eq!(
            parse(&argv(&["suite", "--gaps", "GAPS.md"])).unwrap(),
            Command::Suite {
                strict: false,
                allowlist: None,
                gaps: Some(PathBuf::from("GAPS.md")),
                json: false,
                out: None,
            }
        );
    }

    #[test]
    fn allowlist_requires_strict_and_the_options_need_values() {
        let err = parse(&argv(&["suite", "--allowlist", "known-gaps.txt"])).unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("requires '--strict'"), "{err}");

        for args in [
            vec!["suite", "--strict", "--allowlist"],
            vec!["suite", "--strict", "--allowlist="],
            vec!["suite", "--gaps"],
            vec!["suite", "--gaps="],
        ] {
            let err = parse(&argv(&args)).unwrap_err();
            assert!(matches!(err, Error::Usage(_)), "{args:?}: {err:?}");
        }
    }

    #[test]
    fn allowlist_and_gaps_are_only_accepted_by_suite() {
        for args in [
            vec!["run", "etf/term_to_binary_tuple", "--allowlist", "x"],
            vec!["compare", "1+1", "--allowlist=x"],
            vec!["list", "--gaps", "x"],
            vec!["list", "--gaps=x"],
        ] {
            assert!(
                matches!(parse(&argv(&args)), Err(Error::Usage(_))),
                "{args:?} must be a usage error"
            );
        }
    }

    #[test]
    fn allowlist_and_gaps_are_documented_in_suite_help() {
        assert!(usage(Some("suite")).contains("--allowlist <file>"));
        assert!(usage(Some("suite")).contains("--gaps <path>"));
        assert!(ROOT_USAGE.contains("--allowlist <file>"));
        assert!(ROOT_USAGE.contains("--gaps <path>"));
    }

    #[test]
    fn parse_list_handles_json_and_out() {
        assert_eq!(
            parse(&argv(&["list", "--json"])).unwrap(),
            Command::List {
                by_contract: false,
                json: true,
                out: None,
            }
        );
        assert_eq!(
            parse(&argv(&["list", "--out", "reports"])).unwrap(),
            Command::List {
                by_contract: false,
                json: false,
                out: Some(PathBuf::from("reports")),
            }
        );
    }

    #[test]
    fn parse_list_handles_by_contract() {
        assert_eq!(
            parse(&argv(&["list", "--by-contract"])).unwrap(),
            Command::List {
                by_contract: true,
                json: false,
                out: None,
            }
        );
        // The flag composes with the shared options, in any order, and is idempotent.
        assert_eq!(
            parse(&argv(&["list", "--json", "--by-contract", "--out", "reports"])).unwrap(),
            Command::List {
                by_contract: true,
                json: true,
                out: Some(PathBuf::from("reports")),
            }
        );
        assert_eq!(
            parse(&argv(&["list", "--by-contract", "--by-contract"])).unwrap(),
            Command::List {
                by_contract: true,
                json: false,
                out: None,
            }
        );
    }

    #[test]
    fn by_contract_is_documented_in_list_help() {
        assert!(usage(Some("list")).contains("--by-contract"));
        assert!(ROOT_USAGE.contains("--by-contract"));
    }

    #[test]
    fn strict_is_only_accepted_by_suite() {
        // `--strict` is a suite option; the exit-code contract it drives (unsupported/
        // skipped become failures) lives in the suite path only.
        assert_eq!(
            parse(&argv(&["suite", "--strict", "--strict"])).unwrap(),
            Command::Suite {
                strict: true,
                allowlist: None,
                gaps: None,
                json: false,
                out: None,
            }
        );
        assert!(matches!(
            parse(&argv(&["run", "etf/term_to_binary_tuple", "--strict"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse(&argv(&["list", "--strict"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse(&argv(&["compare", "1+1", "--strict"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn by_contract_is_only_accepted_by_list() {
        assert!(matches!(
            parse(&argv(&["suite", "--by-contract"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse(&argv(&["run", "etf/term_to_binary_tuple", "--by-contract"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn unknown_subcommands_and_options_are_usage_errors() {
        let err = parse(&argv(&["frobnicate"])).unwrap_err();
        assert!(err.to_string().contains("unknown subcommand 'frobnicate'"));
        assert!(err.to_string().contains("USAGE"));
        assert!(matches!(
            parse(&argv(&["suite", "--wat"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(parse(&argv(&["list", "extra"])), Err(Error::Usage(_))));
        assert!(matches!(parse(&argv(&["run", "--out"])), Err(Error::Usage(_))));
        assert!(matches!(
            parse(&argv(&["compare", "--pa"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn lone_dash_is_a_positional() {
        assert_eq!(
            parse(&argv(&["compare", "-"])).unwrap(),
            Command::Compare {
                expr: "-".to_string(),
                pa: Vec::new(),
                json: false,
                out: None,
            }
        );
    }
}
