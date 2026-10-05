//! The command-line surface: the A.6 parser, the usage text and the driver.
//!
//! # The supported subset (the *complete* surface — anything else is an error)
//!
//! | Switch | Meaning |
//! |---|---|
//! | `-version`, `--version` | print the version banner and exit 0 |
//! | `-noshell` | run head-less (Milestone-1 has no interactive shell) |
//! | `-eval Expr` | evaluate the documented integer subset and print the result |
//! | `-s Mod Func [Args…]` | apply `Mod:Func(Args…)` with **atom** arguments; the result is **discarded** (real `erl` semantics) |
//! | `-pa Dir` | add `Dir` to the code path (searched **before** `-pz` directories) |
//! | `-pz Dir` | add `Dir` to the code path (searched after) |
//! | `+S N[:N]` | normal scheduler count (the first number) |
//! | `+A N` | async-thread pool size, preserved verbatim in `extra_emulator_flags` |
//! | `+P N` | maximum number of processes |
//! | `-name N` / `-sname N` | distribution node name |
//! | `run File Mod:Func/Arity [Int…]` | load `File`, run the function and print the result |
//!
//! A switch that is not in this table is a **parse error naming it** — never silently
//! ignored (contract A.6).
//!
//! # Exit codes
//!
//! `0` success · `1` runtime error (a load/execute/BIF failure) · `2` usage or parse
//! error.

use std::path::{Path, PathBuf};

use beam_bifs::StdoutSink;
use beam_common::Result;
use beam_loader::load_file;
use beam_term::{make_atom, try_make_small, Eterm};

use crate::boot;
use crate::config::{Action, EmulatorConfig, RuntimeConfig, DEFAULT_HEAP_WORDS};
use crate::error;
use crate::runtime::Runtime;

/// The usage text, printed on a parse error.
pub const USAGE: &str = "\
Usage: beam_rt [switch...] [action]

Switches:
  -version, --version       print the version banner and exit
  -noshell                  run head-less (the only Milestone-1 mode)
  -eval EXPR                evaluate an integer expression and print the result
  -s MOD FUNC [ARG...]      apply MOD:FUNC(ARG, ...) with atom arguments (result discarded)
  -pa DIR                   add DIR to the code path (searched before -pz dirs)
  -pz DIR                   add DIR to the code path (searched after -pa dirs)
  +S N[:N]                  number of normal schedulers
  +A N                      async-thread pool size (preserved verbatim)
  +P N                      maximum number of processes
  -name N, -sname N         distribution node name

Actions:
  run FILE MOD:FUNC/ARITY [INT...]
                            load FILE, run MOD:FUNC/ARITY and print the result";

/// The exit code of a successful run.
pub const EXIT_OK: i32 = 0;
/// The exit code of a runtime error (load/execute/BIF failure).
pub const EXIT_RUNTIME: i32 = 1;
/// The exit code of a usage or parse error.
pub const EXIT_USAGE: i32 = 2;

/// Parses `args` (which **exclude** `argv[0]`) into a [`RuntimeConfig`].
///
/// # Errors
///
/// A message-carrying [`beam_common::BeamError`] naming the offending switch or
/// argument; the caller prints it together with [`USAGE`] and exits
/// [`EXIT_USAGE`].
pub fn parse(args: &[String]) -> Result<RuntimeConfig> {
    let mut builder = Builder {
        emulator: EmulatorConfig::default(),
        pa: Vec::new(),
        pz: Vec::new(),
        action: Action::Boot,
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "-version" | "--version" => {
                builder.action = Action::Version;
                index += 1;
            }
            "-noshell" => {
                index += 1;
            }
            "-eval" => {
                let expr = value_of(args, index, "-eval")?;
                builder.action = Action::Eval(expr);
                index += 2;
            }
            "-pa" => {
                builder.pa.push(PathBuf::from(value_of(args, index, "-pa")?));
                index += 2;
            }
            "-pz" => {
                builder.pz.push(PathBuf::from(value_of(args, index, "-pz")?));
                index += 2;
            }
            "-name" | "-sname" => {
                builder.emulator.node_name = Some(value_of(args, index, argument)?);
                index += 2;
            }
            "+S" => {
                let value = value_of(args, index, "+S")?;
                builder.emulator.schedulers = Some(parse_schedulers(&value)?);
                index += 2;
            }
            "+A" => {
                let value = value_of(args, index, "+A")?;
                // The async-thread pool is not modelled by the runtime core; it is
                // preserved **verbatim** so nothing is lost (the A.6 skeleton contract).
                parse_u32("+A", &value)?;
                builder.emulator.extra_emulator_flags.push("+A".to_string());
                builder.emulator.extra_emulator_flags.push(value);
                index += 2;
            }
            "+P" => {
                let value = value_of(args, index, "+P")?;
                builder.emulator.max_processes = Some(parse_usize("+P", &value)?);
                index += 2;
            }
            "-s" => index = parse_apply(args, index, &mut builder)?,
            "run" => index = parse_run(args, index, &mut builder)?,
            other if is_switch(other) => return Err(error::unknown_switch(other)),
            other => return Err(error::unexpected_argument(other)),
        }
    }
    let code_paths = builder.code_paths();
    Ok(RuntimeConfig {
        emulator: builder.emulator,
        code_paths,
        heap_words: DEFAULT_HEAP_WORDS,
        action: builder.action,
    })
}

/// The parser's accumulator.
struct Builder {
    /// The emulator start-up surface being assembled.
    emulator: EmulatorConfig,
    /// `-pa` directories, in order.
    pa: Vec<PathBuf>,
    /// `-pz` directories, in order.
    pz: Vec<PathBuf>,
    /// The selected action.
    action: Action,
}

impl Builder {
    /// The code path: `-pa` directories first, then `-pz` directories.
    fn code_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.pa.clone();
        paths.extend(self.pz.iter().cloned());
        paths
    }
}

/// Whether `argument` looks like an emulator switch.
fn is_switch(argument: &str) -> bool {
    argument.starts_with('-') || argument.starts_with('+')
}

/// The value following the switch at `index`.
fn value_of(args: &[String], index: usize, switch: &str) -> Result<String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| error::missing_value(switch))
}

/// Parses a `u32` switch value.
fn parse_u32(switch: &str, value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .map_err(|_| error::bad_switch_value(switch, value))
}

/// Parses a `usize` switch value.
fn parse_usize(switch: &str, value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| error::bad_switch_value(switch, value))
}

/// Parses the `+S N[:N]` value, taking the normal-scheduler count (the first number).
///
/// Both components must be numbers (and there may be at most one `:`): a malformed
/// suffix is an error rather than something silently ignored.
fn parse_schedulers(value: &str) -> Result<u32> {
    let mut parts = value.split(':');
    let normal = parse_u32("+S", parts.next().unwrap_or(value))?;
    if let Some(online) = parts.next() {
        parse_u32("+S", online)?;
    }
    if parts.next().is_some() {
        return Err(error::bad_switch_value("+S", value));
    }
    Ok(normal)
}

/// Parses `-s Mod Func [Args…]`, returning the index of the next switch.
///
/// The arguments run until the next switch (an argument starting with `-` or `+`), as
/// in real `erl`.
fn parse_apply(args: &[String], index: usize, builder: &mut Builder) -> Result<usize> {
    let module = args
        .get(index + 1)
        .cloned()
        .ok_or_else(|| error::missing_value("-s"))?;
    let function = args
        .get(index + 2)
        .cloned()
        .ok_or_else(|| error::missing_value("-s"))?;
    let mut arguments = Vec::new();
    let mut next = index + 3;
    while let Some(candidate) = args.get(next) {
        if is_switch(candidate) {
            break;
        }
        arguments.push(candidate.clone());
        next += 1;
    }
    builder.action = Action::Apply {
        module,
        function,
        args: arguments,
    };
    Ok(next)
}

/// Parses `run File Mod:Func/Arity [Int…]`, returning the index past the last argument.
fn parse_run(args: &[String], index: usize, builder: &mut Builder) -> Result<usize> {
    let file = args
        .get(index + 1)
        .cloned()
        .ok_or_else(|| error::missing_value("run"))?;
    let spec = args
        .get(index + 2)
        .cloned()
        .ok_or_else(|| error::missing_value("run"))?;
    let (module, function, arity) = split_spec(&spec)?;

    let mut arguments = Vec::new();
    for raw in &args[index + 3..] {
        let value = raw
            .parse::<i64>()
            .map_err(|_| error::unexpected_argument(raw))?;
        arguments.push(
            try_make_small(value)
                .map_err(|_| error::bad_switch_value(&spec, raw))?,
        );
    }
    if arguments.len() != arity as usize {
        return Err(error::arity_mismatch(&spec, arity, arguments.len()));
    }
    builder.action = Action::RunFile {
        file: PathBuf::from(file),
        module,
        function,
        arity,
        args: arguments,
    };
    Ok(args.len())
}

/// Splits `module:function/arity`.
fn split_spec(spec: &str) -> Result<(String, String, u32)> {
    let (module, tail) = spec.split_once(':').ok_or_else(|| error::bad_spec(spec))?;
    let (function, arity) = tail.split_once('/').ok_or_else(|| error::bad_spec(spec))?;
    if module.is_empty() || function.is_empty() {
        return Err(error::bad_spec(spec));
    }
    let arity = arity.parse::<u32>().map_err(|_| error::bad_spec(spec))?;
    Ok((module.to_string(), function.to_string(), arity))
}

/// The CLI driver: parse, run, report. Returns the process exit code.
///
/// `args` excludes `argv[0]`. Diagnostics go to stderr; program output (the `~p`
/// printer, `io:format`) goes to stdout.
pub fn run(args: Vec<String>) -> i32 {
    let config = match parse(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("beam_rt: {error}");
            eprintln!("{USAGE}");
            return EXIT_USAGE;
        }
    };
    match drive(config) {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("beam_rt: {error}");
            EXIT_RUNTIME
        }
    }
}

/// Dispatches the parsed action against a fresh runtime.
fn drive(config: RuntimeConfig) -> Result<()> {
    let action = config.action.clone();
    match action {
        Action::Version => {
            println!("{}", boot::boot_banner());
            Ok(())
        }
        Action::Boot => {
            let mut runtime = Runtime::new(config)?;
            runtime.boot()
        }
        Action::Eval(expr) => {
            let mut runtime = Runtime::new(config)?;
            let value = runtime.eval(&expr)?;
            let text = runtime.format_term(value, None)?;
            print!("{text}");
            Ok(())
        }
        Action::Apply {
            module,
            function,
            args,
        } => {
            let mut runtime = Runtime::new(config)?;
            apply(&mut runtime, &module, &function, &args)
        }
        Action::RunFile {
            file,
            module,
            function,
            arity,
            args,
        } => {
            let mut runtime = Runtime::new(config)?;
            run_file(&mut runtime, &file, &module, &function, arity, &args)
        }
    }
}

/// The `-s Mod Func [Args…]` action: atom arguments, result discarded (contract A.6).
fn apply(runtime: &mut Runtime, module: &str, function: &str, args: &[String]) -> Result<()> {
    let path = match runtime.find_module_file(module) {
        Some(path) => path,
        None => {
            return Err(error::module_not_found(
                module,
                &runtime.config().code_paths,
            ))
        }
    };
    let image = load_file(&path)?;
    let mut arguments = Vec::with_capacity(args.len());
    for name in args {
        let atom = image
            .atom_table
            .lookup(name)
            .ok_or_else(|| error::unknown_atom(module, name))?;
        arguments.push(make_atom(atom));
    }
    let mut sink = StdoutSink;
    // The result is discarded — exactly what `erl -s` does.
    let _result = runtime.run_module(&image, function, arguments.len() as u32, &arguments, &mut sink)?;
    Ok(())
}

/// The `run File Mod:Func/Arity [Int…]` action: run and **print** the result.
fn run_file(
    runtime: &mut Runtime,
    file: &Path,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<()> {
    let result = runtime.run_function(file, function, arity, args)?;
    if runtime.module(module).is_none() {
        return Err(error::unknown_module(module));
    }
    let text = runtime.format_term(result, None)?;
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use beam_term::signed_value;

    use super::*;

    fn parse_args(args: &[&str]) -> Result<RuntimeConfig> {
        let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        parse(&owned)
    }

    fn parsed(args: &[&str]) -> RuntimeConfig {
        parse_args(args).unwrap_or_else(|error| panic!("{args:?} must parse: {error}"))
    }

    #[test]
    fn no_arguments_boots() {
        let config = parsed(&[]);
        assert_eq!(config.action, Action::Boot);
        assert!(config.code_paths.is_empty());
        assert_eq!(config.heap_words, DEFAULT_HEAP_WORDS);
    }

    #[test]
    fn version_switches() {
        for flag in ["-version", "--version"] {
            let config = parsed(&[flag]);
            assert_eq!(config.action, Action::Version);
            assert!(config.is_version());
        }
    }

    #[test]
    fn noshell_is_accepted_and_ignored() {
        assert_eq!(parsed(&["-noshell"]).action, Action::Boot);
        assert_eq!(parsed(&["-noshell", "-noshell"]).action, Action::Boot);
    }

    #[test]
    fn eval_switch() {
        let config = parsed(&["-eval", "2+3"]);
        assert_eq!(config.action, Action::Eval("2+3".to_string()));
        let config = parsed(&["-noshell", "-eval", "(1+2)*3"]);
        assert_eq!(config.action, Action::Eval("(1+2)*3".to_string()));
        assert!(parse_args(&["-eval"]).is_err(), "-eval needs a value");
    }

    #[test]
    fn code_path_switches_order_pa_before_pz() {
        let config = parsed(&["-pa", "/a", "-pz", "/b", "-pa", "/c"]);
        assert_eq!(
            config.code_paths,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/c"),
                PathBuf::from("/b")
            ]
        );
        assert!(parse_args(&["-pa"]).is_err());
        assert!(parse_args(&["-pz"]).is_err());
    }

    #[test]
    fn emulator_switches_are_mapped() {
        let config = parsed(&["+S", "4"]);
        assert_eq!(config.emulator.schedulers, Some(4));
        let config = parsed(&["+S", "8:4"]);
        assert_eq!(config.emulator.schedulers, Some(8));
        let config = parsed(&["+P", "1000"]);
        assert_eq!(config.emulator.max_processes, Some(1000));
        let config = parsed(&["+A", "16"]);
        assert_eq!(
            config.emulator.extra_emulator_flags,
            vec!["+A".to_string(), "16".to_string()]
        );
        assert_eq!(config.emulator.schedulers, None);
        assert!(parse_args(&["+S", "x"]).is_err());
        assert!(parse_args(&["+S", "1:x"]).is_err());
        assert!(parse_args(&["+P", "-1"]).is_err());
        assert!(parse_args(&["+A", "many"]).is_err());
        assert!(parse_args(&["+S"]).is_err());
    }

    #[test]
    fn node_name_switches() {
        let config = parsed(&["-name", "beam@host"]);
        assert_eq!(config.emulator.node_name.as_deref(), Some("beam@host"));
        let config = parsed(&["-sname", "beam"]);
        assert_eq!(config.emulator.node_name.as_deref(), Some("beam"));
        assert!(parse_args(&["-name"]).is_err());
    }

    #[test]
    fn apply_switch_collects_atom_arguments_until_the_next_switch() {
        let config = parsed(&["-s", "init", "stop", "a", "b", "+S", "2"]);
        assert_eq!(
            config.action,
            Action::Apply {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: vec!["a".to_string(), "b".to_string()],
            }
        );
        assert_eq!(config.emulator.schedulers, Some(2));

        let config = parsed(&["-s", "init", "stop"]);
        assert_eq!(
            config.action,
            Action::Apply {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: Vec::new(),
            }
        );
        assert!(parse_args(&["-s", "init"]).is_err(), "-s needs a function");
        assert!(parse_args(&["-s"]).is_err());
    }

    #[test]
    fn run_action_parses_the_spec_and_integer_arguments() {
        let config = parsed(&["run", "slice.beam", "slice:add/2", "2", "3"]);
        match config.action {
            Action::RunFile {
                file,
                module,
                function,
                arity,
                args,
            } => {
                assert_eq!(file, PathBuf::from("slice.beam"));
                assert_eq!(module, "slice");
                assert_eq!(function, "add");
                assert_eq!(arity, 2);
                assert_eq!(args.len(), 2);
                assert_eq!(signed_value(args[0]), 2);
                assert_eq!(signed_value(args[1]), 3);
            }
            other => panic!("expected a run action, got {other:?}"),
        }
    }

    #[test]
    fn run_action_validates_the_spec_and_the_argument_count() {
        for args in [
            &["run", "slice.beam", "slice-add-2"][..],
            &["run", "slice.beam", "slice:add"][..],
            &["run", "slice.beam", "slice:add/x", "1", "2"][..],
            &["run", "slice.beam", ":add/2", "1", "2"][..],
            &["run", "slice.beam", "slice:/2", "1", "2"][..],
            &["run", "slice.beam"][..],
            &["run"][..],
        ] {
            assert!(parse_args(args).is_err(), "{args:?} must be rejected");
        }
        // Wrong number of integer arguments.
        assert!(parse_args(&["run", "slice.beam", "slice:add/2", "2"]).is_err());
        assert!(parse_args(&["run", "slice.beam", "slice:add/2", "2", "3", "4"]).is_err());
        // A non-integer argument.
        assert!(parse_args(&["run", "slice.beam", "slice:add/2", "2", "x"]).is_err());
        // A negative integer is a legal argument (not a switch) when arity matches.
        let config = parsed(&["run", "slice.beam", "slice:add/2", "2", "-3"]);
        match config.action {
            Action::RunFile { args, .. } => assert_eq!(signed_value(args[1]), -3),
            other => panic!("expected a run action, got {other:?}"),
        }
    }

    #[test]
    fn unknown_switches_are_errors_naming_the_switch() {
        let error = parse_args(&["--nope"]).expect_err("unknown switch");
        assert!(error.to_string().contains("--nope"), "{error}");
        let error = parse_args(&["+Weird"]).expect_err("unknown switch");
        assert!(error.to_string().contains("+Weird"), "{error}");
        let error = parse_args(&["-emu_flavor", "emu"]).expect_err("out of scope");
        assert!(error.to_string().contains("-emu_flavor"), "{error}");
        let error = parse_args(&["junk"]).expect_err("stray argument");
        assert!(error.to_string().contains("junk"), "{error}");
    }

    #[test]
    fn usage_text_documents_every_supported_switch() {
        for spelling in [
            "-version",
            "-noshell",
            "-eval",
            "-s ",
            "-pa",
            "-pz",
            "+S",
            "+A",
            "+P",
            "-name",
            "run ",
        ] {
            assert!(USAGE.contains(spelling), "USAGE must mention {spelling}");
        }
    }

    #[test]
    fn exit_codes_are_distinct() {
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_RUNTIME, 1);
        assert_eq!(EXIT_USAGE, 2);
    }

    #[test]
    fn version_action_runs_without_a_runtime() {
        assert_eq!(run(vec!["--version".to_string()]), EXIT_OK);
    }

    #[test]
    fn a_parse_error_exits_with_the_usage_code() {
        assert_eq!(run(vec!["--nope".to_string()]), EXIT_USAGE);
    }

    #[test]
    fn eval_action_dispatches_through_the_registry() {
        assert_eq!(run(vec!["-eval".to_string(), "2+3".to_string()]), EXIT_OK);
    }
}
