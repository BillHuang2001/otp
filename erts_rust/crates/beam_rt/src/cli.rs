//! The command-line surface: the A.6 parser, the usage text and the driver.
//!
//! # The supported subset
//!
//! | Switch | Meaning |
//! |---|---|
//! | `-version`, `--version` | print the version banner and exit 0 |
//! | `-noshell` | run head-less (Milestone-1 has no interactive shell) |
//! | `-eval Expr` | evaluate the documented integer subset and print **nothing** |
//! | `-s Mod Func [Args…]` | apply `Mod:Func(Args…)` with **atom** arguments; the result is **discarded** (real `erl` semantics) |
//! | `-run Mod Func [Args…]` | apply `Mod:Func(Args…)` with **string** (char list) arguments; the result is **discarded** |
//! | `-pa Dir` | add `Dir` to the code path (searched **before** `-pz` directories) |
//! | `-pz Dir` | add `Dir` to the code path (searched after) |
//! | `+S N[:N]` | normal scheduler count (the first number) |
//! | `+A N` | async-thread pool size, preserved verbatim in `extra_emulator_flags` |
//! | `+P N` | maximum number of processes |
//! | `-name N` / `-sname N` | distribution node name |
//! | `run File Mod:Func/Arity [Args…]` | load `File`, run the function and print the result |
//!
//! # Unknown switches are init arguments, not errors
//!
//! A switch that is **not** in the table above is **not** a usage error: real `erl`
//! hands every command-line flag to `init:get_argument/1` (verified:
//! `erl -noshell --nope -eval 'halt(0).'` exits 0). So an unrecognized `-flag`/`+flag`
//! is recorded as an init argument — its name the flag with the leading `-`/`+` run
//! removed, its values the following tokens that do not themselves start with `-`/`+` —
//! and parsing continues. Only a **stray positional argument** (a token that is neither
//! a switch, nor part of an action) is a usage error.
//!
//! Every *recognized* switch is recorded the same way ([`InitArgs`]), so a program can
//! observe its own command line: `-noshell` is one valueless occurrence
//! (`init:get_argument(noshell)` ⇒ `{ok,[[]]}`) and `-pa /a -pa /b` is two occurrences
//! (`{ok,[["/a"],["/b"]]}`).
//!
//! # The `run` argument grammar
//!
//! `run FILE MOD:FUNC/ARITY ARG…` parses each `ARG` token as one of:
//!
//! * a **signed small integer** (`42`, `-3`) → [`ArgTerm::Int`];
//! * a **double-quoted string** (`"text\n"`) → [`ArgTerm::Str`], materialized as an
//!   Erlang char list; the escapes `\n`, `\t`, `\r`, `\\` and `\"` are honoured and any
//!   other backslash sequence is rejected;
//! * a **list** (`[1, atom, "x", [2]]`) → [`ArgTerm::List`] of recursively parsed
//!   arguments (top-level commas separate elements; whitespace is tolerated);
//! * anything else → [`ArgTerm::Atom`], a bare atom name interned when materialized.
//!
//! The materialized argument count must equal the declared arity. A token outside this
//! grammar (an empty token, an unterminated string or list, a non-integer that starts
//! with `-`/`+`) is a usage error naming the token.
//!
//! # Exit codes (the stable external contract)
//!
//! | Code | Meaning | stderr |
//! |---|---|---|
//! | `0` | success | — |
//! | `1` | genuine runtime error (a load/execute/BIF failure) | `beam_rt: <error>` |
//! | `2` | malformed invocation / usage error | `beam_rt: <error>` then [`USAGE`] |
//! | `3` | unsupported (capability gap) | exactly one line, `beam_rt: unsupported: <reason>` |
//!
//! The `beam_rt: unsupported: ` prefix (with its trailing space) is the signal the
//! differential harness keys off; it is emitted on **no** other path, and the
//! unsupported path prints no usage text. See [`error::unsupported_reason`] for how a
//! capability gap is classified. A runtime **halt** (`erlang:halt/0,1` executed by the
//! program) is neither: it exits with the requested status printing nothing.
//!
//! `-eval`, `-s`, `-run` and `run` are **silent on stderr** on success.

use std::path::{Path, PathBuf};

use beam_common::{BeamError, Result};
use beam_term::Eterm;

use crate::boot;
use crate::config::{Action, ArgTerm, EmulatorConfig, InitArgs, RuntimeConfig, DEFAULT_HEAP_WORDS};
use crate::error;
use crate::runtime::Runtime;

/// The usage text, printed on a usage (exit 2) error.
pub const USAGE: &str = "\
Usage: beam_rt [switch...] [action]

Switches:
  -version, --version       print the version banner and exit
  -noshell                  run head-less (the only Milestone-1 mode)
  -eval EXPR                evaluate the documented integer subset (prints nothing)
  -s MOD FUNC [ARG...]      apply MOD:FUNC(ARG, ...) with atom arguments (result discarded)
  -run MOD FUNC [ARG...]    apply MOD:FUNC(ARG, ...) with string arguments (result discarded)
  -pa DIR                   add DIR to the code path (searched before -pz dirs)
  -pz DIR                   add DIR to the code path (searched after -pa dirs)
  +S N[:N]                  number of normal schedulers
  +A N                      async-thread pool size (preserved verbatim)
  +P N                      maximum number of processes
  -name N, -sname N         distribution node name
  any other -FLAG [VALUE...] is recorded as an init argument (init:get_argument/1)

Actions:
  run FILE MOD:FUNC/ARITY [ARG...]
                            load FILE, run MOD:FUNC/ARITY and print the result

run arguments (any of):
  42, -3                    a small integer
  atom                      a bare atom name
  \"text\\n\"                  a string (an Erlang char list; \\n \\t \\r \\\\ \\\" escapes)
  [1, atom, \"x\", [2]]       a list of run arguments

Exit codes:
  0  success        1  runtime error        2  usage error        3  unsupported";

/// The exit code of a successful run.
pub const EXIT_OK: i32 = 0;
/// The exit code of a genuine runtime error (a load/execute/BIF failure).
pub const EXIT_RUNTIME: i32 = 1;
/// The exit code of a malformed invocation / usage error.
pub const EXIT_USAGE: i32 = 2;
/// The exit code of an **unsupported** operation (a Milestone-1 capability gap).
///
/// Printed to stderr as the single line `beam_rt: unsupported: <reason>`. Distinct
/// from [`EXIT_RUNTIME`] (a genuine failure) and [`EXIT_USAGE`] (a malformed
/// invocation) so the differential harness can tell "not implemented yet" apart from
/// "broken" and "badly invoked".
pub const EXIT_UNSUPPORTED: i32 = 3;

/// Parses `args` (which **exclude** `argv[0]`) into a [`RuntimeConfig`].
///
/// Every recognized switch is recorded, in order, as an [`InitArgs`] occurrence (the
/// source of `init:get_argument/1`), as is every *unrecognized* `-flag`/`+flag` — real
/// `erl` accepts unmodeled switches as init arguments, so they are never a usage error.
///
/// # Errors
///
/// A message-carrying [`beam_common::BeamError`] naming the offending switch or
/// argument. A **missing operand of a recognized switch**, a **malformed value**, a
/// **malformed `run` spec/argument** and a **stray positional argument** are genuine
/// usage errors (reported with [`USAGE`], exit code [`EXIT_USAGE`]). A malformed
/// `-eval` expression is *not* decided here — it is a capability gap detected when the
/// expression is evaluated (exit code [`EXIT_UNSUPPORTED`], see [`crate::eval`]).
pub fn parse(args: &[String]) -> Result<RuntimeConfig> {
    let mut builder = Builder {
        emulator: EmulatorConfig::default(),
        pa: Vec::new(),
        pz: Vec::new(),
        init_args: InitArgs::new(),
        action: Action::Boot,
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "-version" | "--version" => {
                builder.record(argument, Vec::new());
                builder.action = Action::Version;
                index += 1;
            }
            "-noshell" => {
                builder.record(argument, Vec::new());
                index += 1;
            }
            "-eval" => {
                let expr = value_of(args, index, "-eval")?;
                builder.record(argument, vec![expr.clone()]);
                builder.action = Action::Eval(strip_trailing_dot(&expr));
                index += 2;
            }
            "-pa" => {
                let value = value_of(args, index, "-pa")?;
                builder.record(argument, vec![value.clone()]);
                builder.pa.push(PathBuf::from(value));
                index += 2;
            }
            "-pz" => {
                let value = value_of(args, index, "-pz")?;
                builder.record(argument, vec![value.clone()]);
                builder.pz.push(PathBuf::from(value));
                index += 2;
            }
            "-name" | "-sname" => {
                let value = value_of(args, index, argument)?;
                builder.record(argument, vec![value.clone()]);
                builder.emulator.node_name = Some(value);
                index += 2;
            }
            "+S" => {
                let value = value_of(args, index, "+S")?;
                builder.emulator.schedulers = Some(parse_schedulers(&value)?);
                builder.record(argument, vec![value]);
                index += 2;
            }
            "+A" => {
                let value = value_of(args, index, "+A")?;
                // The async-thread pool is not modelled by the runtime core; it is
                // preserved **verbatim** so nothing is lost (the A.6 skeleton contract).
                parse_u32("+A", &value)?;
                builder.emulator.extra_emulator_flags.push("+A".to_string());
                builder.emulator.extra_emulator_flags.push(value.clone());
                builder.record(argument, vec![value]);
                index += 2;
            }
            "+P" => {
                let value = value_of(args, index, "+P")?;
                builder.emulator.max_processes = Some(parse_usize("+P", &value)?);
                builder.record(argument, vec![value]);
                index += 2;
            }
            "-s" => index = builder.parse_apply(args, index, "-s")?,
            "-run" => index = builder.parse_apply(args, index, "-run")?,
            "run" => index = parse_run_file(args, index, &mut builder)?,
            other if is_switch(other) => index = builder.record_unmodeled(args, index),
            other => return Err(error::unexpected_argument(other)),
        }
    }
    let code_paths = builder.code_paths();
    Ok(RuntimeConfig {
        emulator: builder.emulator,
        code_paths,
        heap_words: DEFAULT_HEAP_WORDS,
        init_args: builder.init_args,
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
    /// Every command-line flag, in order (the `init:get_argument/1` source).
    init_args: InitArgs,
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

    /// Records one occurrence of `switch` (the flag name is the switch without its
    /// leading `-`/`+`) with `values`.
    fn record(&mut self, switch: &str, values: Vec<String>) {
        self.init_args.record(flag_name(switch), values);
    }

    /// Parses `-s Mod Func [Args…]` / `-run Mod Func [Args…]`, returning the index of
    /// the next switch. `-s` collects **atom** arguments, `-run` **string** arguments —
    /// exactly as real `erl` treats them; the difference only shows when the terms are
    /// materialized by the driver.
    fn parse_apply(&mut self, args: &[String], index: usize, switch: &str) -> Result<usize> {
        let module = args
            .get(index + 1)
            .cloned()
            .ok_or_else(|| error::missing_value(switch))?;
        let function = args
            .get(index + 2)
            .cloned()
            .ok_or_else(|| error::missing_value(switch))?;
        let (arguments, next) = collect_tail(args, index + 3);
        let mut values = vec![module.clone(), function.clone()];
        values.extend(arguments.iter().cloned());
        self.record(switch, values);
        self.action = if switch == "-s" {
            Action::Apply {
                module,
                function,
                args: arguments,
            }
        } else {
            Action::Run {
                module,
                function,
                args: arguments,
            }
        };
        Ok(next)
    }

    /// Records an unrecognized `-flag`/`+flag` and its values as an init argument and
    /// returns the index of the next switch — the real `erl` behaviour.
    ///
    /// This is **not** an error: `erl -noshell --nope -eval 'halt(0).'` exits 0, so an
    /// unmodeled switch is data, not a malformed invocation. The values are the
    /// following tokens that do not themselves start with `-` or `+`.
    fn record_unmodeled(&mut self, args: &[String], index: usize) -> usize {
        let (values, next) = collect_tail(args, index + 1);
        self.record(&args[index], values);
        next
    }
}

/// Whether `argument` looks like an emulator switch.
fn is_switch(argument: &str) -> bool {
    argument.starts_with('-') || argument.starts_with('+')
}

/// The flag name of `switch`: the switch with its leading `-`/`+` run removed
/// (`--nope` → `nope`, `+Weird` → `Weird`, `-pa` → `pa`).
fn flag_name(switch: &str) -> &str {
    switch.trim_start_matches(|c| c == '-' || c == '+')
}

/// The tokens from `from` up to (but not including) the next switch, plus the index of
/// that next switch (or the end of the argument list).
fn collect_tail(args: &[String], from: usize) -> (Vec<String>, usize) {
    let mut values = Vec::new();
    let mut next = from;
    while let Some(candidate) = args.get(next) {
        if is_switch(candidate) {
            break;
        }
        values.push(candidate.clone());
        next += 1;
    }
    (values, next)
}

/// The value following the switch at `index`.
fn value_of(args: &[String], index: usize, switch: &str) -> Result<String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| error::missing_value(switch))
}

/// `-eval` tolerates **one** trailing `.` (the shell-style statement terminator):
/// `-eval '2+3.'` evaluates `2+3`.
fn strip_trailing_dot(expr: &str) -> String {
    match expr.strip_suffix('.') {
        Some(head) => head.to_string(),
        None => expr.to_string(),
    }
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

/// Parses `run File Mod:Func/Arity [Arg…]`, returning the index past the last argument.
///
/// The arguments run to the end of the command line (the `run` action is terminal), and
/// every token is parsed by [`parse_arg_term`]; the materialized count must match the
/// declared arity.
fn parse_run_file(args: &[String], index: usize, builder: &mut Builder) -> Result<usize> {
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
        arguments.push(parse_arg_term(raw)?);
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

/// Parses one `run` argument token (the grammar documented in the module docs).
///
/// # Errors
///
/// [`error::bad_run_argument`] for an empty token, an unterminated/ill-formed string or
/// list, an unknown string escape, or a `-`/`+`-prefixed token that is not an integer.
fn parse_arg_term(token: &str) -> Result<ArgTerm> {
    match token.as_bytes().first() {
        None => Err(error::bad_run_argument(token)),
        Some(b'"') => parse_string(token),
        Some(b'[') => parse_list(token),
        Some(b'-') | Some(b'+') | Some(b'0'..=b'9') => token
            .parse::<i64>()
            .map(ArgTerm::Int)
            .map_err(|_| error::bad_run_argument(token)),
        Some(_) => Ok(ArgTerm::Atom(token.to_string())),
    }
}

/// Parses a `"…"` token: the quotes are stripped and `\n`, `\t`, `\r`, `\\` and `\"`
/// are unescaped.
fn parse_string(token: &str) -> Result<ArgTerm> {
    let inner = token
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|_| token.len() >= 2)
        .ok_or_else(|| error::bad_run_argument(token))?;
    let mut text = String::new();
    let mut chars = inner.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('r') => text.push('\r'),
            Some('\\') => text.push('\\'),
            Some('"') => text.push('"'),
            Some(other) => {
                return Err(error::bad_run_argument(&format!(
                    "{token} (unknown escape \\{other})"
                )))
            }
            None => return Err(error::bad_run_argument(token)),
        }
    }
    Ok(ArgTerm::Str(text))
}

/// Parses a `[…]` token: a comma-separated list of recursively parsed arguments, with
/// whitespace tolerated around the elements.
fn parse_list(token: &str) -> Result<ArgTerm> {
    let inner = token
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .filter(|_| token.len() >= 2)
        .ok_or_else(|| error::bad_run_argument(token))?;
    if inner.trim().is_empty() {
        return Ok(ArgTerm::List(Vec::new()));
    }
    let mut elements = Vec::new();
    for part in split_top_level(token, inner)? {
        let part = part.trim();
        if part.is_empty() {
            return Err(error::bad_run_argument(token));
        }
        elements.push(parse_arg_term(part)?);
    }
    Ok(ArgTerm::List(elements))
}

/// Splits the inside of a `[…]` token on the commas that sit outside nested brackets
/// and quoted strings, so `[[1,2],3]` is two elements and not three.
///
/// # Errors
///
/// [`error::bad_run_argument`] for an unbalanced `]` or an unterminated string.
fn split_top_level<'a>(token: &'a str, inner: &'a str) -> Result<Vec<&'a str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in inner.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        match character {
            '"' => quoted = true,
            '[' => depth += 1,
            ']' => match depth.checked_sub(1) {
                Some(remaining) => depth = remaining,
                None => return Err(error::bad_run_argument(token)),
            },
            ',' if depth == 0 => {
                parts.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted || depth != 0 {
        return Err(error::bad_run_argument(token));
    }
    parts.push(&inner[start..]);
    Ok(parts)
}

/// The CLI driver: parse, run, report. Returns the process exit code.
///
/// `args` excludes `argv[0]`. Diagnostics go to stderr; program output (the `~w`
/// printer, `io:format`) goes to stdout.
///
/// The exit code follows the documented contract (module docs): `0` success, `1` a
/// genuine runtime error, `2` a usage error (with [`USAGE`]) and `3` an unsupported
/// capability gap — on which a single `beam_rt: unsupported: <reason>` line is printed
/// and no usage text. A **halt** the program requested wins over all of them: the
/// runtime exits with that status, printing nothing.
pub fn run(args: Vec<String>) -> i32 {
    let config = match parse(&args) {
        Ok(config) => config,
        Err(error) => {
            if let Some(reason) = error::unsupported_reason(&error) {
                return report_unsupported(&reason);
            }
            eprintln!("beam_rt: {error}");
            eprintln!("{USAGE}");
            return EXIT_USAGE;
        }
    };
    match drive(config) {
        Ok(()) => EXIT_OK,
        Err(error) => {
            if let Some(status) = error::halt_status(&error) {
                // The program asked to terminate with this status: no diagnostic.
                return status;
            }
            if let Some(reason) = error::unsupported_reason(&error) {
                return report_unsupported(&reason);
            }
            eprintln!("beam_rt: {error}");
            EXIT_RUNTIME
        }
    }
}

/// Reports a capability gap on stderr and returns [`EXIT_UNSUPPORTED`].
///
/// Prints **exactly one** line — `beam_rt: unsupported: <reason>` — and no usage text;
/// this stable signal is what the differential harness keys off.
fn report_unsupported(reason: &str) -> i32 {
    eprintln!("beam_rt: unsupported: {reason}");
    EXIT_UNSUPPORTED
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
            runtime.boot()?;
            // Real `erl -eval '2+3'` prints nothing and exits 0: the expression is
            // evaluated for its effect only (`halt`, a start script, …).
            let _value = runtime.eval(&expr)?;
            Ok(())
        }
        Action::Apply {
            module,
            function,
            args,
        } => {
            let mut runtime = Runtime::new(config)?;
            let mut terms = Vec::with_capacity(args.len());
            for name in &args {
                terms.push(runtime.intern(name));
            }
            apply(&mut runtime, &module, &function, &terms)
        }
        Action::Run {
            module,
            function,
            args,
        } => {
            let mut runtime = Runtime::new(config)?;
            let parsed: Vec<ArgTerm> = args.into_iter().map(ArgTerm::Str).collect();
            let terms = runtime.materialize_args(&parsed)?;
            apply(&mut runtime, &module, &function, &terms)
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

/// The `-s Mod Func [Args…]` / `-run Mod Func [Args…]` action: the result is
/// **discarded** (real `erl` semantics), so a failure is still reported but a value is
/// not printed.
fn apply(runtime: &mut Runtime, module: &str, function: &str, args: &[Eterm]) -> Result<()> {
    let _result = runtime.run_named(module, function, args.len() as u32, args)?;
    Ok(())
}

/// The `run File Mod:Func/Arity [Args…]` action: run and **print** the result.
///
/// The result is rendered with `~w~n` — exactly what the oracle's
/// `io:format("~w~n",[V])` produces — and the loaded module's name must be the one the
/// spec named (a mismatch means the file is not the requested module).
///
/// An **uncaught raise** leaves through [`reported_reason`], which renders a
/// structured reason with its real term (see there).
fn run_file(
    runtime: &mut Runtime,
    file: &Path,
    module: &str,
    function: &str,
    arity: u32,
    args: &[ArgTerm],
) -> Result<()> {
    let terms = runtime.materialize_args(args)?;
    let result = match runtime.run_function(file, function, arity, &terms) {
        Ok(result) => result,
        Err(error) => return Err(reported_reason(runtime, error)),
    };
    if runtime.module(module).is_none() {
        return Err(error::unknown_module(module));
    }
    let text = runtime.format_term_w(result, None)?;
    print!("{text}");
    Ok(())
}

/// The diagnostic [`run`] prints for an **uncaught raise**: the raise with its real
/// reason.
///
/// `BeamError` renders a raise as `<class> <reason>` ([`std::fmt::Display`]), and
/// `beam_common` cannot render a term — so a raise whose reason is a **structured**
/// term (`{badkey, K}`, `{badmap, M}`, `{case_clause, V}`, …) is
/// [`ExitReason::Structured`](beam_common::ExitReason::Structured) and would print the
/// placeholder `<structured reason>`. The term itself travels in the raise's
/// live-reason slot, so it is recovered with [`error::live_reason_term`] and rendered
/// through the runtime's one printer (the registered `~w`): the reported line becomes
/// `beam_rt: error {badkey,nokey}` — the same `error <reason>` shape an atom reason
/// already has.
///
/// A raise that is **not** structured is returned unchanged, which keeps every other
/// classification exactly as it was:
///
/// * an **atom** reason (`undef`, `function_clause`, …) already renders as
///   `error <atom>`;
/// * a **halt** and a **capability gap** are classified by [`run`] *before* the
///   diagnostic — and a structured reason is a genuine `error`-class raise, so it is
///   never one of them.
///
/// A term the printer itself rejects (a printer limitation, not a runtime error) leaves
/// the original raise in place: the failure is still reported, never replaced by an
/// invented reason.
fn reported_reason(runtime: &mut Runtime, error: BeamError) -> BeamError {
    let Some(term) = error::live_reason_term(&error) else {
        return error;
    };
    match runtime.format_term_w(term, None) {
        // `~w~n` ends the rendering with a newline; the CLI's `eprintln!` adds its own.
        Ok(text) => error::message(text.trim_end_matches('\n')),
        Err(_) => error,
    }
}

#[cfg(test)]
mod tests {
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
        assert!(config.init_args.is_empty());
    }

    #[test]
    fn version_switches() {
        for flag in ["-version", "--version"] {
            let config = parsed(&[flag]);
            assert_eq!(config.action, Action::Version);
            assert!(config.is_version());
            assert_eq!(config.init_args.get("version"), Some(vec![&[][..]]));
        }
    }

    #[test]
    fn noshell_is_accepted_and_recorded() {
        assert_eq!(parsed(&["-noshell"]).action, Action::Boot);
        assert_eq!(parsed(&["-noshell", "-noshell"]).action, Action::Boot);
        // `init:get_argument(noshell)` is `{ok,[[]]}`: two occurrences, no values.
        let config = parsed(&["-noshell", "-noshell"]);
        assert_eq!(
            config.init_args.get("noshell"),
            Some(vec![&[][..], &[][..]])
        );
    }

    #[test]
    fn eval_switch() {
        let config = parsed(&["-eval", "2+3"]);
        assert_eq!(config.action, Action::Eval("2+3".to_string()));
        let config = parsed(&["-noshell", "-eval", "(1+2)*3"]);
        assert_eq!(config.action, Action::Eval("(1+2)*3".to_string()));
        assert!(parse_args(&["-eval"]).is_err(), "-eval needs a value");
        assert_eq!(
            config.init_args.get("eval"),
            Some(vec![&["(1+2)*3".to_string()][..]])
        );
    }

    #[test]
    fn eval_tolerates_one_trailing_dot() {
        assert_eq!(
            parsed(&["-eval", "2+3."]).action,
            Action::Eval("2+3".to_string())
        );
        // Only the last one is a terminator; a leading dot is part of the expression.
        assert_eq!(
            parsed(&["-eval", "2+3.."]).action,
            Action::Eval("2+3.".to_string())
        );
        // The recorded init argument is the raw token.
        assert_eq!(
            parsed(&["-eval", "2+3."]).init_args.get("eval"),
            Some(vec![&["2+3.".to_string()][..]])
        );
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
        // Every `-pa` occurrence is recorded, in command-line order.
        assert_eq!(
            config.init_args.get("pa"),
            Some(vec![&["/a".to_string()][..], &["/c".to_string()][..]])
        );
        assert_eq!(
            config.init_args.get("pz"),
            Some(vec![&["/b".to_string()][..]])
        );
    }

    #[test]
    fn emulator_switches_are_mapped() {
        let config = parsed(&["+S", "4"]);
        assert_eq!(config.emulator.schedulers, Some(4));
        assert_eq!(config.init_args.get("S"), Some(vec![&["4".to_string()][..]]));
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
        assert_eq!(config.init_args.get("A"), Some(vec![&["16".to_string()][..]]));
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
        assert_eq!(
            config.init_args.get("sname"),
            Some(vec![&["beam".to_string()][..]])
        );
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
        assert_eq!(
            config.init_args.get("s"),
            Some(vec![&[
                "init".to_string(),
                "stop".to_string(),
                "a".to_string(),
                "b".to_string()
            ][..]])
        );

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
    fn run_switch_takes_string_arguments() {
        let config = parsed(&["-run", "init", "stop", "a", "b", "+S", "2"]);
        assert_eq!(
            config.action,
            Action::Run {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: vec!["a".to_string(), "b".to_string()],
            }
        );
        assert_eq!(config.emulator.schedulers, Some(2));
        assert_eq!(config.init_args.get("run").map(|found| found.len()), Some(1));
        assert!(parse_args(&["-run", "init"]).is_err(), "-run needs a function");
        assert!(parse_args(&["-run"]).is_err());
    }

    #[test]
    fn run_action_parses_the_spec_and_the_argument_grammar() {
        let config = parsed(&[
            "run",
            "mod.beam",
            "multimod:f/5",
            "42",
            "-3",
            "an_atom",
            "\"a\\tb\"",
            "[1, b, \"c\", [2]]",
        ]);
        match config.action {
            Action::RunFile {
                file,
                module,
                function,
                arity,
                args,
            } => {
                assert_eq!(file, PathBuf::from("mod.beam"));
                assert_eq!(module, "multimod");
                assert_eq!(function, "f");
                assert_eq!(arity, 5);
                assert_eq!(
                    args,
                    vec![
                        ArgTerm::Int(42),
                        ArgTerm::Int(-3),
                        ArgTerm::Atom("an_atom".to_string()),
                        ArgTerm::Str("a\tb".to_string()),
                        ArgTerm::List(vec![
                            ArgTerm::Int(1),
                            ArgTerm::Atom("b".to_string()),
                            ArgTerm::Str("c".to_string()),
                            ArgTerm::List(vec![ArgTerm::Int(2)]),
                        ]),
                    ]
                );
            }
            other => panic!("expected a run action, got {other:?}"),
        }
    }

    #[test]
    fn run_argument_tokens_are_parsed_individually() {
        assert_eq!(parse_arg_term("2").expect("int"), ArgTerm::Int(2));
        assert_eq!(parse_arg_term("-3").expect("negative int"), ArgTerm::Int(-3));
        assert_eq!(
            parse_arg_term("x").expect("atom"),
            ArgTerm::Atom("x".to_string())
        );
        assert_eq!(
            parse_arg_term("\"hi\\n\"").expect("string"),
            ArgTerm::Str("hi\n".to_string())
        );
        assert_eq!(
            parse_arg_term("\"q\\\\ \\\"\"").expect("escapes"),
            ArgTerm::Str("q\\ \"".to_string())
        );
        assert_eq!(parse_arg_term("[]").expect("empty list"), ArgTerm::List(vec![]));
        assert_eq!(
            parse_arg_term("[[1,2],3]").expect("nested list"),
            ArgTerm::List(vec![
                ArgTerm::List(vec![ArgTerm::Int(1), ArgTerm::Int(2)]),
                ArgTerm::Int(3),
            ])
        );
        // Malformed tokens are usage errors, never silently reinterpreted.
        for token in [
            "",
            "-pa",
            "+x",
            "[1,",
            "[1,,2]",
            "[1",
            "\"unterminated",
            "\"bad\\q\"",
            "[[1]",
        ] {
            assert!(
                parse_arg_term(token).is_err(),
                "{token:?} must be rejected"
            );
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
        // Wrong number of arguments (materialized terms must match the arity).
        assert!(parse_args(&["run", "slice.beam", "slice:add/2", "2"]).is_err());
        assert!(parse_args(&["run", "slice.beam", "slice:add/2", "2", "3", "4"]).is_err());
        // A non-integer token is a legal **atom** argument.
        let config = parsed(&["run", "slice.beam", "slice:add/2", "2", "x"]);
        match config.action {
            Action::RunFile { args, .. } => assert_eq!(
                args,
                vec![ArgTerm::Int(2), ArgTerm::Atom("x".to_string())]
            ),
            other => panic!("expected a run action, got {other:?}"),
        }
        // A negative integer is a legal argument (not a switch) when arity matches.
        let config = parsed(&["run", "slice.beam", "slice:add/2", "2", "-3"]);
        match config.action {
            Action::RunFile { args, .. } => {
                assert_eq!(args, vec![ArgTerm::Int(2), ArgTerm::Int(-3)]);
                assert_eq!(
                    args.iter()
                        .map(|arg| match arg {
                            ArgTerm::Int(value) => *value,
                            other => panic!("expected an integer, got {other:?}"),
                        })
                        .sum::<i64>(),
                    -1
                );
            }
            other => panic!("expected a run action, got {other:?}"),
        }
    }

    #[test]
    fn unknown_switches_are_recorded_as_init_arguments() {
        // Real `erl` accepts unmodeled switches as init args, so they parse.
        let config = parsed(&["--nope"]);
        assert_eq!(config.action, Action::Boot);
        assert_eq!(config.init_args.get("nope"), Some(vec![&[][..]]));

        let config = parsed(&["+Weird"]);
        assert!(config.init_args.get("Weird").is_some());

        let config = parsed(&["-emu_flavor", "emu"]);
        assert_eq!(
            config.init_args.get("emu_flavor"),
            Some(vec![&["emu".to_string()][..]])
        );

        // Values stop at the next switch, and parsing continues with it.
        let config = parsed(&["-a", "1", "2", "+S", "3"]);
        assert_eq!(
            config.init_args.get("a"),
            Some(vec![&["1".to_string(), "2".to_string()][..]])
        );
        assert_eq!(config.emulator.schedulers, Some(3));

        // A stray positional argument is still a *usage* error, not a capability gap.
        let error = parse_args(&["junk"]).expect_err("stray argument");
        assert!(error.to_string().contains("junk"), "{error}");
        assert_eq!(error::unsupported_reason(&error), None);
    }

    #[test]
    fn usage_text_documents_every_supported_switch() {
        for spelling in [
            "-version",
            "-noshell",
            "-eval",
            "-s ",
            "-run",
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
        assert_eq!(EXIT_UNSUPPORTED, 3);
    }

    #[test]
    fn version_action_runs_without_a_runtime() {
        assert_eq!(run(vec!["--version".to_string()]), EXIT_OK);
    }

    #[test]
    fn a_usage_error_exits_with_the_usage_code() {
        // A genuine usage error: a missing operand of a known switch and a stray
        // positional argument.
        assert_eq!(run(vec!["-eval".to_string()]), EXIT_USAGE);
        assert_eq!(run(vec!["junk".to_string()]), EXIT_USAGE);
    }

    #[test]
    fn an_unsupported_operation_exits_with_the_unsupported_code() {
        // An `-eval` expression outside the supported grammar (drive path) …
        assert_eq!(
            run(vec!["-eval".to_string(), "1+".to_string()]),
            EXIT_UNSUPPORTED
        );
        // … while an unmodeled switch is a recorded init argument, not a gap.
        assert_eq!(run(vec!["--nope".to_string()]), EXIT_OK);
    }

    #[test]
    fn eval_action_dispatches_through_the_registry() {
        assert_eq!(run(vec!["-eval".to_string(), "2+3".to_string()]), EXIT_OK);
        assert_eq!(
            run(vec!["-noshell".to_string(), "-eval".to_string(), "2*3.".to_string()]),
            EXIT_OK
        );
    }

    #[test]
    fn a_halt_is_reported_as_the_process_status() {
        // The halt raise is what the driver turns into the exit status; assert the
        // classification the driver keys off (a real halt needs executing code).
        let error = error::halt(42);
        assert_eq!(error::halt_status(&error), Some(42));
        assert_eq!(error::unsupported_reason(&error), None);
    }

    #[test]
    fn a_missing_file_is_a_runtime_error_not_a_usage_error() {
        assert_eq!(
            run(vec![
                "run".to_string(),
                "/nonexistent/nope.beam".to_string(),
                "nope:f/0".to_string(),
            ]),
            EXIT_RUNTIME
        );
    }
}
