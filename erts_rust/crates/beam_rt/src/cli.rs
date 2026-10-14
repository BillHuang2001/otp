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
//! | `-smp enable\|disable\|auto` | SMP scheduler model (only `disable` changes behaviour) |
//! | `+A N` | async-thread pool size, preserved verbatim in `extra_emulator_flags` |
//! | `+P N` | maximum number of processes |
//! | `+hms N` | minimum heap size, in words (rounded through the growth table) |
//! | `+hmbs N` | minimum binary virtual heap size, in words (likewise rounded) |
//! | `+hmax N` | maximum heap size, in words (`0` = unlimited) |
//! | `+hmaxk B` / `+hmaxel B` / `+hmaxib B` | the `+hmax` kill / error-logger / shared-binary flags (`true`\|`false`) |
//! | `-name N` / `-sname N` | distribution node name |
//! | `-setcookie C` | distribution cookie (recorded as an init argument; sets `emulator.cookie`) |
//! | `-config F` | config file, recorded as the init argument `config` **and** resolved/read-checked by every booting action (a missing file is a runtime error, exit 1) |
//! | `-boot F` | boot script: recorded as the init argument `boot` **and** executed by the A.7 boot-script executor (the `Boot` action) |
//! | `-emu_flavor F` | emulator flavor: `emu` ⇒ interpreter, `jit`/`smp` ⇒ JIT (recorded) |
//! | `+Q N` | maximum number of ports (`legacy`, or `1024..=134217727`) |
//! | `+K true\|false` | kernel-poll switch |
//! | `+sbt TYPE` | scheduler bind type (`u`/`db`/`s`/`ps`/`ts`/`tnnps`/`nnps`/`nnts`/`ns`) |
//! | `-M*` / `+MM*` / `+MB*` | ERTS allocator flags (the `+M*` forms are validated; all preserved verbatim) |
//! | `+JPperf VALUE` | perf support: `true`\|`false`\|`dump`\|`map`\|`fp`\|`no_fp` (contract A.8; see [`crate::perf_map`]) |
//! | `+JPperfdirectory Dir` | where the perf files are written (default `/tmp`); `ERL_SYM_MAP_FILE` overrides the map's whole path |
//! | `run File Mod:Func/Arity [Args…]` | load `File`, run the function and print the result |
//!
//! # `ERL_*FLAGS`
//!
//! The [`run`] driver first merges the `ERL_*FLAGS` environment variables into the
//! command line, in real `erl`'s order (`initial_argv_massage`):
//! `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]` — so a
//! tokenized environment flag is indistinguishable from a command-line flag to
//! `init:get_argument/1`. See [`parse_with_env`] and [`env`].
//!
//! A **second** environment family is read directly by the emulator instead of being merged:
//! [`vars`] (`ERL_MAX_PORTS`, `ERL_CRASH_DUMP`, `ERL_INETRC`, `ERL_ROOTDIR`, …) plus the
//! perf map's `ERL_SYM_MAP_FILE`. None of them is an init argument; the two with a modelled
//! effect route into the configuration ([`parse_with_env`]).
//!
//! # Init arguments (`init:get_argument/1`), exactly as the oracle
//!
//! A switch that is **not** in the table above is **not** a usage error (real `erl`
//! accepts unmodeled switches: `erl -noshell --nope -eval 'halt(0).'` exits 0), and
//! only a **stray positional argument** is a usage error.
//!
//! Which switches become **init arguments** follows the oracle exactly: a **`-`-switch**
//! (recognized or not) is recorded, while a **`+`-flag** and the `-eval`/`-s`/`-run`
//! Start instructions are consumed by the emulator/`init` and are **not** (the oracle's
//! `init:get_argument('S'|'Q'|eval|s|run)` is the bare atom `error`; verified against
//! OTP 29.1.1 / erts-17.1). So `-noshell` is one valueless occurrence
//! (`init:get_argument(noshell)` ⇒ `{ok,[[]]}`) and `-pa /a -pa /b` is two occurrences
//! (`{ok,[["/a"],["/b"]]}`). The oracle additionally reports its own
//! `root`/`bindir`/`progname`/`home` entries, which this runtime does not model.
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
use beam_term::{Eterm, MAX_SMALL};

use crate::boot;
use crate::config::{
    Action, ArgTerm, EmulatorConfig, HeapOptions, InitArgs, RuntimeConfig, DEFAULT_HEAP_WORDS,
};
use crate::crash_dump;
use crate::error;
use crate::perf_map::{self, PerfMapConfig};
use crate::runtime::Runtime;

mod env;
pub(crate) mod flags;
mod vars;
pub use env::{merge_argv, otp_flags_var, tokenize, Env, MapEnv, ProcessEnv};
pub use vars::{from_env, ErlEnv};

/// The usage text, printed on a usage (exit 2) error.
pub const USAGE: &str = "\
Usage: beam_rt [switch...] [action]

Switches:
  -version, --version       print the version banner and exit
  -noshell                  run head-less (the only Milestone-1 mode)
  -eval EXPR                evaluate the documented integer subset (prints nothing)
  -s MOD FUNC [ARG...]      apply MOD:FUNC(ARG, ...) with atom arguments (result discarded)
  -run MOD FUNC [ARG...]    apply MOD:FUNC(ARG, ...) with string arguments (result discarded)
  -pa DIR [DIR...]          add DIR(s) to the head of the code path (before the system
                            directories; a missing dir is filtered out)
  -pz DIR [DIR...]          add DIR(s) to the tail of the code path (after the system
                            directories; a missing dir is filtered out)
  +S N[:N]                  normal schedulers (total, and optionally the online count)
  -smp enable|disable|auto  SMP scheduler model (only 'disable' changes behaviour)
  +A N                      async-thread pool size (preserved verbatim)
  +P N                      maximum number of processes (legacy, or 1024..=134217727)
  +Q N                      maximum number of ports (legacy, or 1024..=134217727)
  +K true|false             kernel-poll switch
  +sbt TYPE                 scheduler bind type (u|db|s|ps|ts|tnnps|nnps|nnts|ns)
  -MNAME[=VALUE]            ERTS allocator flag (also +MM.../+MB...; preserved verbatim)
  +hms N                    minimum heap size, in words
  +hmbs N                   minimum binary virtual heap size, in words
  +hmax N                   maximum heap size, in words (0 = unlimited)
  +hmaxk true|false         kill the process at +hmax (default true)
  +hmaxel true|false        log an error report at +hmax (default true)
  +hmaxib true|false        count shared binaries towards +hmax (default false)
  -name N, -sname N         distribution node name
  -setcookie C              distribution cookie
  -config FILE              configuration file (recorded as init:get_argument(config))
  -boot FILE                boot script: executed by the boot action (a bare invocation);
                            also recorded as init:get_argument(boot)
  -emu_flavor emu|jit|smp   emulator flavor
  any other -FLAG [VALUE...] is recorded as an init argument (init:get_argument/1);
                            a +FLAG is an emulator flag and is not an init argument

Environment (merged into the command line, real erl's order):
  ERL_OTP<ver>_FLAGS, ERL_AFLAGS, ERL_FLAGS, ERL_ZFLAGS
Environment (read by the emulator/launcher; never an init argument):
  ERL_MAX_PORTS (clamped into 1024..=134217727, then applied as +Q when no +Q is given)
  ERL_INETRC, ERL_CRASH_DUMP, ERL_CRASH_DUMP_SECONDS, ERL_ROOTDIR,
  ERL_MAX_ETS_TABLES, ERL_FULLSWEEP_AFTER (parsed and reported, inert in this milestone)

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

/// Parses `args` merged with the `ERL_*FLAGS` environment variables.
///
/// Real `erl` merges four environment variables into the command line before `init`
/// sees it (`erts/etc/common/erlexec.c`, `initial_argv_massage`):
/// `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]`. The
/// merge is [`env::merge_argv`] and the result is handed to the **pure** [`parse`], so an
/// environment flag is indistinguishable from a command-line flag (contract A.6).
///
/// The **second** environment family — the variables the emulator (and the `erl` launcher)
/// read directly, outside that merge — is read into [`RuntimeConfig::env`] by
/// [`vars::from_env`]. None of them is an init argument (verified against OTP 29.1.1 /
/// erts-17.1; see [`vars`]).
///
/// # `ERL_MAX_PORTS`
///
/// `ERL_MAX_PORTS` is the one member of that family with a modelled effect: the emulator
/// `atoi`s it and clamps it into `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]` (`erl_init.c:1341`).
/// The clamped value requests a port table exactly as `+Q N` does, so it is applied
/// **twice** — as a `+Q` entry of
/// [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags) (nothing is
/// lost from the emulator-flag surface) and as the
/// [`max_ports`](RuntimeConfig::max_ports) request the runtime reports through
/// `system_info(port_limit)` — and only when the merged command line carries **no** `+Q`
/// of its own. A command-line or `ERL_FLAGS` `+Q` therefore wins outright, exactly as in
/// the oracle, where `ERL_MAX_PORTS=2048 erl +Q 4096` and `ERL_FLAGS='+Q 4096' erl` both
/// observe a `port_limit` of 4096.
///
/// # `ERL_SYM_MAP_FILE`
///
/// `ERL_SYM_MAP_FILE` belongs to that family too — the emulator's perf-map writer reads it
/// with `erts_sys_explicit_host_getenv` (`beam_jit_metadata.cpp`), so it is neither merged
/// into the command line nor recorded as an `init` argument — but it *is* a modelled effect:
/// it overrides the perf map's whole path, so it is folded into
/// [`perf_map`](RuntimeConfig::perf_map) here (contract A.8). It selects no file of its own:
/// like the C, it is consulted only where a `+JPperf map`/`true` file would be created.
///
/// [`parse`] itself never reads the process environment: the differential harness and the
/// unit tests call it directly, and [`run`] calls this function with [`ProcessEnv`].
///
/// # Errors
///
/// Exactly the errors [`parse`] reports for the merged command line.
pub fn parse_with_env(args: &[String], env: &dyn Env) -> Result<RuntimeConfig> {
    let merged = env::merge_argv(args, env);
    let mut config = parse(&merged)?;
    let env_vars = vars::from_env(env);
    // `ERL_MAX_PORTS` acts exactly like a `+Q` the command line does not already carry
    // (the oracle: any `+Q` in the merged argv overrides the variable). The two tokens go
    // into `extra_emulator_flags` through the same primitive `+Q` uses, and the value
    // itself becomes the configuration's port-table request.
    if let Some(max_ports) = env_vars.max_ports {
        if !vars::has_plus_q_switch(&merged) {
            preserve_emulator_flag(&mut config.emulator, "+Q", &max_ports.to_string());
            config.max_ports = Some(max_ports);
        }
    }
    // `ERL_SYM_MAP_FILE` is the perf map's path override (contract A.8): the emulator reads
    // it directly — outside the `ERL_*FLAGS` merge, and never as an `init` argument — and it
    // applies only to the map file (`+JPperf map`/`true`). Like `ERL_MAX_PORTS` above it is
    // folded into the configuration here, so the pure `parse` stays environment-free.
    if let Some(path) = env.get(perf_map::SYM_MAP_FILE_VAR) {
        config.perf_map.set_file(path);
    }
    config.env = env_vars;
    Ok(config)
}

/// Parses `args` (which **exclude** `argv[0]`) into a [`RuntimeConfig`].
///
/// A **`-`-switch** is recorded, in order, as an [`InitArgs`] occurrence (the source of
/// `init:get_argument/1`) — as is an *unrecognized* `-flag`. A **`+`-flag**, and the
/// `-eval`/`-s`/`-run` Start instructions, are consumed by the emulator/`init` and are
/// **not** init arguments (verified against OTP 29.1.1 / erts-17.1, where
/// `init:get_argument('S'|'Q'|eval|s|run)` is the bare atom `error`). An unmodeled
/// switch is never a usage error.
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
        online_schedulers: None,
        heap: HeapOptions::new(),
        max_ports: None,
        init_args: InitArgs::new(),
        perf_map: PerfMapConfig::new(),
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
                // `-eval` is a **Start instruction**, not an init argument: the oracle
                // consumes it in `init`'s `parse_boot_args/1`, so
                // `init:get_argument(eval)` is the bare atom `error` (verified against
                // OTP 29.1.1 / erts-17.1).
                builder.action = Action::Eval(strip_trailing_dot(&expr));
                index += 2;
            }
            "-pa" => {
                let (values, next) = path_values(args, index, "-pa")?;
                builder.record(argument, values.clone());
                builder.pa.extend(values.into_iter().map(PathBuf::from));
                index = next;
            }
            "-pz" => {
                let (values, next) = path_values(args, index, "-pz")?;
                builder.record(argument, values.clone());
                builder.pz.extend(values.into_iter().map(PathBuf::from));
                index = next;
            }
            "-name" | "-sname" => {
                let value = value_of(args, index, argument)?;
                builder.record(argument, vec![value.clone()]);
                builder.emulator.node_name = Some(value);
                index += 2;
            }
            "-setcookie" => {
                // `-setcookie` takes a list of values exactly like `-pa`/`-pz`: the oracle
                // records one occurrence carrying *every* following non-switch token
                // (`erl -setcookie a b …` ⇒ `init:get_argument(setcookie)` is
                // `{ok,[["a","b"]]}`), and with anything but a single value it leaves the
                // cookie unset (`erlang:get_cookie()` is `nocookie`) — verified against
                // OTP 29.1.1 / erts-17.1.
                let (values, next) = path_values(args, index, "-setcookie")?;
                builder.record(argument, values.clone());
                builder.emulator.cookie = match values.as_slice() {
                    [only] => Some(only.clone()),
                    _ => None,
                };
                index = next;
            }
            "-env" => {
                // `-env VAR VALUE` is consumed by the *launcher* (`erlexec`), so `init`
                // never sees it: the oracle's `init:get_argument(env)` is the bare atom
                // `error` — verified against OTP 29.1.1 / erts-17.1. The two values are
                // taken blindly (the oracle consumes the following token even when it
                // looks like a switch, e.g. `-noshell`).
                index = parse_env_switch(args, index)?;
            }
            "-config" | "-boot" => {
                let value = value_of(args, index, argument)?;
                builder.record(argument, vec![value]);
                index += 2;
            }
            "-emu_flavor" => {
                let value = value_of(args, index, "-emu_flavor")?;
                builder.record(argument, vec![value.clone()]);
                let flavor = flags::parse_emu_flavor("-emu_flavor", &value)?;
                builder.emulator.features =
                    beam_common::FeatureFlags::for_flavor(flavor);
                builder.emulator.flavor = flavor;
                index += 2;
            }
            "+Q" => {
                let value = value_of(args, index, "+Q")?;
                builder.max_ports = flags::parse_max_ports("+Q", &value)?;
                builder.preserve("+Q", &value);
                index += 2;
            }
            "+K" => {
                let value = value_of(args, index, "+K")?;
                flags::parse_kernel_poll("+K", &value)?;
                builder.preserve("+K", &value);
                index += 2;
            }
            "+sbt" => {
                let value = value_of(args, index, "+sbt")?;
                flags::parse_sbt("+sbt", &value)?;
                builder.preserve("+sbt", &value);
                index += 2;
            }
            // `+t N` — the atom-table size (`erl_init.c:2217-2234`); the value is
            // validated and preserved verbatim, so `atom_limit` can follow it
            // (`crate::info::limits`). Like every `+`-flag it is not an init argument.
            "+t" => {
                let value = value_of(args, index, "+t")?;
                flags::parse_atom_table_size("+t", &value)?;
                builder.preserve("+t", &value);
                index += 2;
            }
            // `+e N` — the maximum number of ETS tables (`erl_init.c:1696-1707`); only a
            // negative value is rejected; `ets_limit` follows the value (see
            // `crate::info::limits`).
            "+e" => {
                let value = value_of(args, index, "+e")?;
                flags::parse_ets_tables("+e", &value)?;
                builder.preserve("+e", &value);
                index += 2;
            }
            // `+B [c|d|i]` (and the in-token `+Bi`/`+Bc`/`+Bd`) — the break-handler switch
            // (`erl_init.c:1822-1843`). It never rejects a value: an unrecognized one falls
            // to the `+B` arm (`have_break_handler = 0`). Only `+B i` sets `ignore_break`,
            // which `erlang:system_info(break_ignored)` reports (see `crate::info::system`).
            // The switch and its value are preserved verbatim so that derivation can read
            // them; a valueless `+B` preserves an empty value so the `(switch, value)` stride
            // stays even.
            "+B" => {
                let value = match args.get(index + 1) {
                    Some(candidate) if matches!(candidate.as_str(), "i" | "c" | "d") => {
                        candidate.clone()
                    }
                    _ => String::new(),
                };
                let consumed = !value.is_empty();
                builder.preserve("+B", &value);
                index += if consumed { 2 } else { 1 };
            }
            "+Bi" | "+Bc" | "+Bd" => {
                builder.preserve("+B", &argument[2..]);
                index += 1;
            }
            other if flags::is_allocator_switch(other) => {
                index = flags::parse_allocator(&args[index], args, index)?;
            }
            "+S" => {
                let value = value_of(args, index, "+S")?;
                let (total, online) = parse_schedulers(&value)?;
                builder.emulator.schedulers = total;
                builder.online_schedulers = online;
                index += 2;
            }
            "-smp" => index = parse_smp_switch(args, index, &mut builder),
            "+A" => {
                let value = value_of(args, index, "+A")?;
                // The async-thread pool is not modelled by the runtime core; it is
                // preserved **verbatim** so nothing is lost (the A.6 skeleton contract).
                parse_u32("+A", &value)?;
                builder.preserve("+A", &value);
                index += 2;
            }
            "+P" => {
                let value = value_of(args, index, "+P")?;
                builder.emulator.max_processes = flags::parse_max_processes("+P", &value)?;
                index += 2;
            }
            "+hms" => {
                let value = value_of(args, index, "+hms")?;
                builder.heap.set_min_heap_words(parse_heap_words("+hms", &value)?);
                index += 2;
            }
            "+hmbs" => {
                let value = value_of(args, index, "+hmbs")?;
                builder
                    .heap
                    .set_min_bin_vheap_words(parse_heap_words("+hmbs", &value)?);
                index += 2;
            }
            "+hmax" => {
                let value = value_of(args, index, "+hmax")?;
                builder
                    .heap
                    .set_max_heap_words(parse_max_heap_size(&value)?);
                index += 2;
            }
            "+hmaxk" | "+hmaxel" | "+hmaxib" => {
                let value = value_of(args, index, argument)?;
                builder.set_max_heap_flag(argument, &value)?;
                index += 2;
            }
            // The perf-map / JIT-dump surface (contract A.8). The two switches are emulator
            // flags: they are validated here, preserved verbatim in `extra_emulator_flags`
            // and **never** an `init` argument (like `+Q`/`+K`/`+sbt`). `+JPperfdirectory`'s
            // value is taken verbatim — an unusable directory surfaces as a failed file
            // creation, not as a usage error (`PerfMap::error`), exactly as in the C.
            "+JPperf" => {
                let value = value_of(args, index, "+JPperf")?;
                builder.perf_map.set_flag(&value)?;
                builder.preserve("+JPperf", &value);
                index += 2;
            }
            "+JPperfdirectory" => {
                let value = value_of(args, index, "+JPperfdirectory")?;
                builder.perf_map.set_directory(value.clone());
                builder.preserve("+JPperfdirectory", &value);
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
    let pz_paths = builder.pz_paths();
    Ok(RuntimeConfig {
        emulator: builder.emulator,
        code_paths,
        pz_paths,
        heap_words: DEFAULT_HEAP_WORDS,
        online_schedulers: builder.online_schedulers,
        heap: builder.heap,
        max_ports: builder.max_ports,
        init_args: builder.init_args,
        perf_map: builder.perf_map,
        action: builder.action,
        // The environment family is read only by `parse_with_env`; the pure `parse` sees
        // an empty one (it must never read the process environment).
        env: ErlEnv::default(),
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
    /// The explicit **online** scheduler count (`+S Total:Online`), or `None`.
    online_schedulers: Option<u32>,
    /// The heap-size emulator options (`+hms`/`+hmbs`/`+hmax*`) being assembled.
    heap: HeapOptions,
    /// The requested port-table size (`+Q N`; `None` for the default table or `legacy`).
    max_ports: Option<usize>,
    /// Every command-line flag, in order (the `init:get_argument/1` source).
    init_args: InitArgs,
    /// The perf-map / JIT-dump surface (`+JPperf`/`+JPperfdirectory`) being assembled
    /// (contract A.8).
    perf_map: PerfMapConfig,
    /// The selected action.
    action: Action,
}

impl Builder {
    /// The `-pa` directories, in command-line order — the **head** of the code path.
    ///
    /// The `-pz` directories are the separate tail ([`Builder::pz_paths`]): the oracle's
    /// `code:get_path/0` puts `-pa` in front of the system `ebin` directories and `-pz`
    /// **after** them, so the two groups must not be merged here.
    fn code_paths(&self) -> Vec<PathBuf> {
        self.pa.clone()
    }

    /// The `-pz` directories, in command-line order — the **tail** of the code path.
    fn pz_paths(&self) -> Vec<PathBuf> {
        self.pz.clone()
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
        // `-s`/`-run` are **Start instructions**, not init arguments: `init` consumes
        // them in `parse_boot_args/1`, so `init:get_argument(s|run)` is the bare atom
        // `error` (verified against OTP 29.1.1 / erts-17.1).
        let _ = &arguments;
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

    /// Preserves an emulator switch and its value **verbatim** in
    /// [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags), so
    /// an unmodeled emulator flag is not lost (the A.6 skeleton contract).
    ///
    /// It is deliberately **not** recorded as an init argument: every `+`-flag is an
    /// emulator flag in the oracle, so `init:get_argument('Q'|'K'|'sbt'|'MMscs')` is the
    /// bare atom `error` (verified against OTP 29.1.1 / erts-17.1).
    fn preserve(&mut self, switch: &str, value: &str) {
        preserve_emulator_flag(&mut self.emulator, switch, value);
    }

    /// Applies a `+hmaxk`/`+hmaxel`/`+hmaxib` boolean value to the heap options.
    ///
    /// Only the two literals `true`/`false` are accepted: the C compares the value with
    /// exactly those strings and rejects anything else (`erl_init.c:1592-1624`, e.g.
    /// `+hmaxk bogus` ⇒ `bad max heap kill bogus` + usage).
    fn set_max_heap_flag(&mut self, switch: &str, value: &str) -> Result<()> {
        let mut flags = self.heap.max_heap_flags();
        match (switch, value) {
            ("+hmaxk", "true") => flags.kill = true,
            ("+hmaxk", "false") => flags.kill = false,
            ("+hmaxel", "true") => flags.error_logger = true,
            ("+hmaxel", "false") => flags.error_logger = false,
            ("+hmaxib", "true") => flags.include_shared_binaries = true,
            ("+hmaxib", "false") => flags.include_shared_binaries = false,
            _ => return Err(error::bad_switch_value(switch, value)),
        }
        self.heap.set_max_heap_flags(flags);
        Ok(())
    }

    /// Records an unrecognized `-flag` (and its values) as an init argument and returns
    /// the index of the next switch — the real `erl` behaviour.
    ///
    /// Only an unrecognized **`-`flag** becomes an init argument: `erlexec` forwards
    /// `-x` through `add_arg` (where `init` sees it), while `+x` is rewritten to `-x` and
    /// passed as an **emulator** flag (`erts/etc/common/erlexec.c:871-1102`). So
    /// `init:get_argument(Weird)` is `{ok,[[]]}` for `-Weird` and the bare atom `error`
    /// for `+Weird` (verified: OTP 29.1.1 / erts-17.1). Neither is a usage error:
    /// `erl -noshell --nope +Weird -eval 'halt(0).'` exits 0. The values are the
    /// following tokens that do not themselves start with `-` or `+`.
    fn record_unmodeled(&mut self, args: &[String], index: usize) -> usize {
        let (values, next) = collect_tail(args, index + 1);
        // Only an unrecognized `-flag` is an init argument; a `+flag` is an emulator flag
        // and is invisible to `init:get_argument/1` (see the method's docs).
        if args[index].starts_with('-') {
            self.record(&args[index], values);
        }
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

/// Preserves an emulator switch and its value **verbatim** in
/// [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags): the switch
/// token followed by the value token, in that order.
///
/// Shared by the parser's [`Builder::preserve`] and by [`parse_with_env`]'s
/// `ERL_MAX_PORTS` → `+Q` routing, so both fill the channel identically.
fn preserve_emulator_flag(emulator: &mut EmulatorConfig, switch: &str, value: &str) {
    emulator.extra_emulator_flags.push(switch.to_string());
    emulator.extra_emulator_flags.push(value.to_string());
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

/// Parses a `+hms` / `+hmbs` heap-size value: a **positive** decimal word count.
///
/// Mirrors the emulator's own check: the C parses with `atoi` and rejects any value
/// `<= 0` (`erl_init.c:1554-1567`) — verified against the oracle, where `+hms 0` and
/// `+hms abc` both fail (the emulator prints `bad heap size <value>` and its usage text,
/// exiting 1). `beam_rt` reports its uniform malformed-value diagnostic instead (a usage
/// error: exit 2 + [`USAGE`]). One documented divergence: `atoi` truncates a trailing
/// suffix, so the oracle accepts `+hms 100abc` as 100, while this parser rejects it.
fn parse_heap_words(switch: &str, value: &str) -> Result<usize> {
    match value.parse::<usize>() {
        Ok(words) if words > 0 => Ok(words),
        _ => Err(error::bad_switch_value(switch, value)),
    }
}

/// Parses a `+hmax` value: a non-negative decimal word count no larger than
/// [`MAX_SMALL`], which is exactly the emulator's acceptance range
/// (`erl_init.c:1626-1633`: `hMaxSize < 0 || hMaxSize > MAX_SMALL` is
/// `bad max heap size <value>`; `+hmax 0` — the default — means *unlimited*).
///
/// Verified against the oracle: `+hmax 0` is accepted (unlimited),
/// `+hmax -1` and `+hmax 1000000000000000000000` are rejected. Divergences, both in the
/// *rejection* direction: the C parses with `ErtsStrToSint` and tests only the numeric
/// result, so it silently accepts `+hmax abc` as 0 and `+hmax 12abc` as 12, where this
/// parser rejects both. The C's parse-time "max must not be below the *current* min"
/// check (`erl_init.c:1634-1639`) is **not** modelled: it is order-dependent
/// (`+hmax 500 +hms 1000 +hmax 200` fails, `+hms 2000 +hmax 1000` does not) and only
/// rejects an invocation.
fn parse_max_heap_size(value: &str) -> Result<usize> {
    match value.parse::<i64>() {
        Ok(words) if (0..=MAX_SMALL).contains(&words) => Ok(words as usize),
        _ => Err(error::bad_switch_value("+hmax", value)),
    }
}

/// Parses the `+S N[:N]` value into `(total, online)`.
///
/// Both halves are `Option`: `0` is the oracle's "use the default" spelling, so
/// `+S 0` and `+S 0:0` select the defaults for both counts (`None`/`None`).
///
/// The grammar is `erl_init.c:1107-1153`:
/// * `+S Total` — the online count is **not** the total: the C sets it to
///   `min(Total, <default online>)` (`onln = tot < schdlrs_onln ? tot : schdlrs_onln`),
///   which is why `erl +S 16` reports `16` schedulers but only `14` online on a
///   16-CPU box. That resolution needs the host's processor count, so it happens where
///   the value is answered ([`crate::info`]'s `schedulers_online`), not here.
/// * `+S Total:Online` — the online count is explicit and must be `1..=Total`
///   (`bad amount of schedulers online`); `+S 4:5` and `+S 4:0` are rejected, while
///   `+S 0:2` is accepted (the total defaults to the processor count).
/// * Anything else (`+S 1:2:3`, a non-number) is rejected.
///
/// A malformed value is this parser's uniform usage error (exit 2 + [`USAGE`]); the
/// oracle prints `bad amount of schedulers <value>` and its own usage, exiting 1.
fn parse_schedulers(value: &str) -> Result<(Option<u32>, Option<u32>)> {
    let mut parts = value.split(':');
    let total = parse_u32("+S", parts.next().unwrap_or(value))?;
    let online = match parts.next() {
        Some(online) => Some(parse_u32("+S", online)?),
        None => None,
    };
    if parts.next().is_some() {
        return Err(error::bad_switch_value("+S", value));
    }
    // `0` selects the default of that count; an explicit total of `0` therefore means
    // "the processor count".
    let schedulers = (total > 0).then_some(total);
    let online = match online {
        // `+S 0:0` is the all-defaults spelling.
        Some(0) if total == 0 => None,
        Some(online) if online > 0 => {
            let ceiling = schedulers.unwrap_or_else(|| {
                // The default total the online count can never exceed.
                u32::try_from(crate::info::logical_processors()).unwrap_or(u32::MAX)
            });
            if online > ceiling {
                return Err(error::bad_switch_value("+S", value));
            }
            Some(online)
        }
        Some(_) => return Err(error::bad_switch_value("+S", value)),
        None => None,
    };
    Ok((schedulers, online))
}

/// Collects the **list** of values of a `-pa`/`-pz`/`-setcookie` occurrence, returning
/// them with the index of the next switch.
///
/// The oracle's grammar is "every following token up to the next switch is a value of
/// this one occurrence" — verified against OTP 29.1.1 / erts-17.1: `erl -pa a b …` makes
/// `init:get_argument(pa)` `{ok,[["a","b"]]}` (one occurrence, two values), while
/// `-pa a -3 …` / `-pa a +S 3 …` both stop at the next switch. An occurrence with **no**
/// value is a usage error, exactly as before.
fn path_values(args: &[String], index: usize, switch: &str) -> Result<(Vec<String>, usize)> {
    let (values, next) = collect_path_values(args, index + 1);
    if values.is_empty() {
        return Err(error::missing_value(switch));
    }
    Ok((values, next))
}
/// The tokens from `from` up to (but not including) the next switch **or** the terminal
/// `run` action keyword, plus the index of that stopper.
///
/// This is the value collector for `-pa`/`-pz`/`-setcookie`. It differs from the generic
/// [`collect_tail`] in exactly one way: it also stops at a bare `run` token. `run` is
/// this runtime's own terminal CLI keyword (the vertical slice, see [`parse_run_file`]),
/// so it can never be a directory/cookie *value*; treating it as one would swallow the
/// whole action (`beam_rt -pa DIR run FILE M:F/A` would leave the `Action` at `Boot` and
/// print nothing). Real `erl` has no `run` keyword, so this is a beam_rt-only rule and
/// the oracle's `-pa a b` multi-value grammar is unaffected.
fn collect_path_values(args: &[String], from: usize) -> (Vec<String>, usize) {
    let mut values = Vec::new();
    let mut next = from;
    while let Some(candidate) = args.get(next) {
        if is_switch(candidate) || candidate == "run" {
            break;
        }
        values.push(candidate.clone());
        next += 1;
    }
    (values, next)
}

/// Consumes a `-env VAR VALUE` pair, returning the index of the next switch.
///
/// `-env` is handled by the `erl` *launcher* (`erlexec`), which is why the oracle's
/// `init:get_argument(env)` is the bare atom `error`: the pair never reaches `init` and
/// is therefore **not** an init argument here either (verified against OTP 29.1.1 /
/// erts-17.1). The launcher consumes the next two tokens blindly — `erl -env x -noshell`
/// swallows `-noshell` as the value — so this does too.
///
/// The variable is *not* injected into this process's environment (`std::env::set_var`
/// is `unsafe` in edition 2024 and this crate is `#![deny(unsafe_code)]`), so
/// `os:getenv/1` does not observe it; see the crate's M1 gaps.
fn parse_env_switch(args: &[String], index: usize) -> Result<usize> {
    if args.len() < index + 3 {
        return Err(error::missing_value("-env"));
    }
    Ok(index + 3)
}

/// Parses the `-smp` switch, returning the index of the next switch.
///
/// `-smp` selects the runtime's scheduler model: `disable` turns SMP off
/// ([`FeatureFlags::smp`](beam_common::FeatureFlags::smp) becomes `false`), while
/// `enable`/`auto` leave it on (the default). The value is **optional** and any other
/// value is accepted and recorded but changes nothing — this matches the oracle, which
/// accepts `-smp` (with or without a value) and ignores every value but `disable`
/// (`erl -noshell -smp bogus -eval 'halt().'` exits 0). The one documented divergence is
/// `-smp disable`: real `erl` rejects it outright (`Argument '-smp disable' not supported.
/// Use "+S 1" instead.`) whereas this runtime accepts it as the honest "SMP off" request.
///
/// The switch is recorded as an init argument exactly like every other switch, so a
/// program can still observe its own command line (`init:get_argument(smp)`).
fn parse_smp_switch(args: &[String], index: usize, builder: &mut Builder) -> usize {
    let value = match args.get(index + 1) {
        Some(candidate) if !is_switch(candidate) => Some(candidate.clone()),
        _ => None,
    };
    let has_value = value.is_some();
    if let Some(value) = &value {
        if let Some(enabled) = parse_smp(value) {
            builder.emulator.features = builder.emulator.features.with_smp(enabled);
        }
    }
    builder.record("-smp", value.into_iter().collect());
    if has_value {
        index + 2
    } else {
        index + 1
    }
}

/// The [`FeatureFlags::smp`](beam_common::FeatureFlags::smp) an `-smp` *value* selects, or
/// `None` for a value that is not one of the three documented ones (which changes nothing).
///
/// `enable` and `auto` both mean "SMP on" (the default); `disable` means "SMP off".
fn parse_smp(value: &str) -> Option<bool> {
    match value {
        "enable" | "auto" => Some(true),
        "disable" => Some(false),
        _ => None,
    }
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
///
/// Every **abnormal** exit — a halt that asks for a crash dump (`halt("Slogan")`), an
/// `abort`, and any other runtime failure — writes the environment-named crash dump
/// first (see [`write_crash_dump`]), so the CLI reproduces the oracle's dump-before-exit
/// order (`erts_exit_vv`, `erl_init.c:2751-2755`). A capability gap and a plain status
/// halt write no dump, exactly as the oracle's `ERTS_ERROR_EXIT`/`ERTS_DUMP_EXIT` rules
/// leave them alone.
pub fn run(args: Vec<String>) -> i32 {
    let config = match parse_with_env(&args, &ProcessEnv) {
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
    // `drive` consumes the configuration, but the crash-dump path needs the `ERL_*`
    // environment (`ERL_CRASH_DUMP` / `ERL_CRASH_DUMP_SECONDS`), so keep a copy.
    let env = config.env.clone();
    let mut source = None;
    match drive(config, &mut source) {
        // A successful action still exits with the status a **non-terminating** node stop
        // recorded: `init:stop/0,1` asks the node to halt (`init ! {stop,…}`) and returns
        // `ok` to a caller that keeps running, so the status is applied here — after the
        // whole expression has finished — instead of raising (the `halt/1` path below).
        Ok(()) => source
            .as_ref()
            .and_then(|rt| rt.halt_status())
            .unwrap_or(EXIT_OK),
        Err(error) => {
            if let Some(status) = error::halt_status(&error) {
                // The program asked to terminate with this status: no diagnostic, no dump.
                return status;
            }
            if let Some(reason) = error::unsupported_reason(&error) {
                // A documented Milestone-1 capability gap is not a crash: no dump.
                return report_unsupported(&reason);
            }
            if let Some(slogan) = error::dump_slogan(&error) {
                // `erlang:halt("Slogan")` — the oracle's `ERTS_DUMP_EXIT` (`bif.c:4393`):
                // print the slogan, write the dump, exit 1. The tail is the emulator's own
                // announcement (`erl_crash_dump.c:509`), rendered with the file's name.
                write_crash_dump(source.as_ref(), &env, slogan);
                let path = crash_dump::crash_dump_path(&env)
                    .unwrap_or_else(|| std::path::PathBuf::from("erl_crash.dump"));
                eprint!(
                    "{slogan}\r\n\r\nCrash dump is being written to: {}...done\r\n",
                    path.display()
                );
                return EXIT_RUNTIME;
            }
            // Every other failure is an abnormal exit: the dump comes first, then the
            // termination (`halt(abort)` ends in `abort()`, i.e. SIGABRT — `erl_init.c:2774`).
            write_crash_dump(source.as_ref(), &env, "");
            if error::is_abort(&error) {
                std::process::abort();
            }
            eprintln!("beam_rt: {error}");
            EXIT_RUNTIME
        }
    }
}

/// Writes the environment-named crash dump for an abnormal exit, if dumping is enabled.
///
/// [`crash_dump::crash_dump_path`] decides **whether** a dump is written
/// (`ERL_CRASH_DUMP_SECONDS=0` disables it outright) and **where** it goes
/// (`ERL_CRASH_DUMP`, else `erl_crash.dump` in the current directory) — the oracle's own
/// rule (`erl_crash_dump.c:451-507`). `source` is the runtime the action built: `None`
/// (`-version`, or a runtime that could not even be constructed) means there is nothing to
/// snapshot and the dump is skipped.
///
/// A dump that cannot be produced is reported on stderr and otherwise **ignored**: an
/// unwritable file must not change the exit code or the classification of the original
/// error.
fn write_crash_dump(source: Option<&Runtime>, env: &ErlEnv, slogan: &str) {
    let (Some(runtime), Some(path)) = (source, crash_dump::crash_dump_path(env)) else {
        return;
    };
    let dump = match crash_dump::snapshot(runtime, slogan, env.crash_dump_seconds) {
        Ok(dump) => dump,
        Err(error) => {
            eprintln!("beam_rt: no crash dump: {error}");
            return;
        }
    };
    if let Err(error) = crash_dump::write_dump(&path, &dump) {
        eprintln!("beam_rt: no crash dump: {error}");
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
///
/// The runtime the action built is handed back through `snapshot` — [`run`] needs it to
/// write a crash dump when the action fails abnormally. `Action::Version` builds none
/// (there is nothing to snapshot), and neither does a runtime that could not even be
/// constructed, so the slot stays empty and the dump is skipped.
fn drive(config: RuntimeConfig, snapshot: &mut Option<Runtime>) -> Result<()> {
    let action = config.action.clone();
    // `-config FILE` is read while the node **boots**, before any Start instruction runs: a
    // config file the boot cannot read is the boot failure the oracle reports with exit 1
    // (`erl -noshell -config /nonexistent -eval 'ok'` ⇒ **1**, no stdout, no stderr —
    // verified against OTP 29.1.1 / erts-17.1). Only `-version` builds no runtime at all, so
    // it stays exempt (`erl -version -config /nonexistent` ⇒ **0**, the banner).
    if !matches!(action, Action::Version) {
        check_config_file(&config)?;
    }
    match action {        Action::Version => {
            println!("{}", boot::boot_banner());
            Ok(())
        }
        Action::Boot => {
            // `-boot FILE` is the boot **action**'s script: a bare `beam_rt -boot F` really
            // executes F (contract A.7/A.9) instead of only loading the preloaded set. The
            // value comes from the recorded init arguments, exactly as `init` reads its own
            // `boot` flag, so it must be copied out before `config` moves into the runtime.
            let boot_file = boot_file_argument(&config).map(str::to_string);
            let mut runtime = Runtime::new(config)?;
            let result = match &boot_file {
                Some(value) => run_boot_script(&mut runtime, value),
                None => runtime.boot(),
            };
            // The boot chain's failure is often a **structured** raise reason (the applied
            // directive's own term), which `BeamError`'s `Display` can only render as the
            // placeholder `<structured reason>`. Recover the live term and render it through
            // the runtime's one printer, exactly as the `run` action does — so the reported
            // diagnostic names the directive's real reason instead of a placeholder.
            let result = result.map_err(|error| reported_reason(&mut runtime, error));
            *snapshot = Some(runtime);
            result
        }
        Action::Eval(expr) => {
            let mut runtime = Runtime::new(config)?;
            // Real `erl -eval '2+3'` prints nothing and exits 0: the expression is
            // evaluated for its effect only (`halt`, a start script, …).
            let result = match runtime.boot() {
                Ok(()) => runtime.eval(&expr).map(|_| ()),
                Err(error) => Err(error),
            };
            *snapshot = Some(runtime);
            result
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
            let result = apply(&mut runtime, &module, &function, &terms);
            *snapshot = Some(runtime);
            result
        }
        Action::Run {
            module,
            function,
            args,
        } => {
            let mut runtime = Runtime::new(config)?;
            let parsed: Vec<ArgTerm> = args.into_iter().map(ArgTerm::Str).collect();
            let result = match runtime.materialize_args(&parsed) {
                Ok(terms) => apply(&mut runtime, &module, &function, &terms),
                Err(error) => Err(error),
            };
            *snapshot = Some(runtime);
            result
        }
        Action::RunFile {
            file,
            module,
            function,
            arity,
            args,
        } => {
            let mut runtime = Runtime::new(config)?;
            let result = run_file(&mut runtime, &file, &module, &function, arity, &args);
            *snapshot = Some(runtime);
            result
        }
    }
}

/// The `-boot FILE` drive: resolve `value` the way `init` resolves its `boot` flag and
/// execute the resulting script (contracts A.7/A.9).
///
/// The **boot action** is the one invocation whose whole job is to boot, so it is the one
/// that drives the script; a start instruction (`-eval`/`-s`/`-run`/`run`) keeps the
/// preloaded-only floor, because a start instruction runs code, not the boot script that
/// would start it.
///
/// The runtime boots first: [`Runtime::boot`] is what puts the preloaded set *and* the
/// system code path in place, and the code path is what resolves `$ROOT` — in `value`
/// itself and in the script's own `{path, …}` directives. It is idempotent, and
/// [`Runtime::boot_from_script`] boots again as its own precondition.
///
/// Nothing is swallowed: a file on neither of `init`'s candidates, a malformed script and a
/// directive the runtime cannot execute all return their typed [`BeamError`] — which [`run`]
/// reports as `beam_rt: <error>` with exit code 1 — never a panic, a hang or a wrong result.
///
/// The real `start.boot` does **not** run to completion: it stops at the 0-based directive
/// index **19** of 22, `{apply,{application,start_boot,[{application,stdlib,Props},permanent]}}`,
/// because the stdlib start's own `application_controller` is already gone — the CLI reports
/// `beam_rt: error {noproc,{gen_server,call,[application_controller,{load_application,stdlib},infinity]}}`
/// with exit code 1. The final `{progress,started}` (index 21) is therefore not reached, and
/// neither is the `{apply,{c,erlangrc,[]}}` directive (index 20) whose `{open, …}` of
/// `$HOME/.erlang` the native `file_server_2` service answers. Per-directive detail is in
/// [`crate::boot`].
fn run_boot_script(runtime: &mut Runtime, value: &str) -> Result<()> {    runtime.boot()?;
    let path = resolve_boot_file(value, boot_file_root(runtime).as_deref())?;
    runtime.boot_from_script(&path).map(|_| ())
}

/// Checks the `-config FILE` init argument(s) the way the oracle's boot reads them, so a
/// config file the node cannot read fails the boot instead of being silently ignored.
///
/// # Ground truth (OTP 29.1.1 / erts-17.1)
///
/// `init` hands each `-config` value to the application controller, which resolves it and
/// then `file:consult/1`s the result (`lib/kernel/src/application_controller.erl:1876-1903`,
/// `conf_param_to_conf/1`): `filename:basename(FileName, ".config")` strips a **trailing**
/// `.config` and the name is rebuilt as `<dir>/<base>.config`. So `-config /x/conf` and
/// `-config /x/conf.config` both read `/x/conf.config`. A file that cannot be read throws
/// `{error, {FName, Line, Str}}` out of the boot, which the emulator turns into exit **1**:
///
/// ```text
/// erl -noshell -config /nonexistent      -eval 'ok'  → exit 1   (no stdout, no stderr)
/// erl -noshell -config <dir>.config      -eval 'ok'  → exit 1   (a directory is not a file)
/// erl -noshell -config <existing>.config -eval 'ok'  → exit 0
/// erl -version -config /nonexistent                  → exit 0   (the banner; `-version` never boots)
/// ```
///
/// **Every** occurrence is checked, not just the first: `-config <missing> -config <ok>` and
/// `-config <ok> -config <missing>` both exit 1 in the oracle (verified).
///
/// # Scope
///
/// Only the file's *presence as a regular file* is decided here. A readable file whose
/// contents are malformed (or empty — the oracle's own `sys.config` grammar wants a list of
/// terms) is a **content** failure this runtime does not model, exactly as it does not model
/// the config terms themselves; that residual divergence changes no working invocation (a
/// file `[].`-style boot reads is accepted by both).
///
/// The error is a message-carrying `error`-class raise (`error::message`), which [`run`]
/// reports as `beam_rt: <error>` with [`EXIT_RUNTIME`] — the same classification every other
/// runtime failure takes. No runtime has been built at this point, so [`run`]'s crash-dump
/// path finds an empty snapshot and writes nothing (the oracle writes no dump either).
fn check_config_file(config: &RuntimeConfig) -> Result<()> {
    let Some(occurrences) = config.init_args.get("config") else {
        return Ok(());
    };
    for values in &occurrences {
        for value in values.iter() {
            let resolved = resolve_config_file(value);
            if !resolved.is_file() {
                return Err(error::message(format!(
                    "cannot read config file {}",
                    resolved.display()
                )));
            }
        }
    }
    Ok(())
}
/// Resolves a `-config` value to the file the oracle reads: a trailing `.config` is kept,
/// any other name gets it appended (`application_controller.erl:1877-1879`).
///
/// The rewrite is a **basename + join**, so `-config /a.b/conf` becomes `/a.b/conf.config`
/// and a value already ending in `.config` is unchanged (`filename:basename(X, ".config")`
/// strips at most one occurrence).
fn resolve_config_file(value: &str) -> PathBuf {
    let path = Path::new(value);
    let base = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => format!("{}.config", name.strip_suffix(".config").unwrap_or(name)),
        None => format!("{value}.config"),
    };
    match path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir.join(base),
        None => PathBuf::from(base),
    }
}
/// The `-boot FILE` argument of the merged command line, or `None` when none was given.
///
/// A thin forwarder over [`RuntimeConfig::boot_script`], which owns the semantics (the
/// `init:get_argument(boot)` provenance, first-occurrence-wins, and the "no operand is a
/// usage error" rule).
fn boot_file_argument(config: &RuntimeConfig) -> Option<&str> {
    config.boot_script()
}
/// The Erlang root (`<Root>`) a `-boot` argument is resolved against: the directory above
/// `<lib>` ([`Runtime::code_server`]'s `lib_root`) — the same convention the boot script's
/// own `$ROOT` expansion uses (see [`crate::boot::bootscript`]). `None` until the system
/// install is on the code path, which is the only case that has a `<Root>` to speak of.
fn boot_file_root(runtime: &Runtime) -> Option<PathBuf> {
    let lib_root = runtime.code_server().lib_root()?;
    lib_root.parent().map(Path::to_path_buf)
}

/// Resolves a `-boot FILE` value to the `.boot` file the oracle would read, exactly as
/// `init`'s `get_boot/2` does (`erts/preloaded/src/init.erl:1265-1286`).
///
/// `init` builds one candidate name and then one fallback, in this order:
///
/// 1. `$ROOT/<Name>` becomes `<Root>/bin/<Name>.boot`; any other value is taken verbatim
///    with `.boot` appended — so `-boot start` is `start.boot` (a path relative to the
///    current directory) and `-boot /x/y` is `/x/y.boot`;
/// 2. when (1) does not exist, `<Root>/bin/<candidate>` — a **string concatenation**, as the
///    oracle writes it (`Root ++ "/bin/" ++ BootFile`, `init.erl:1275`), so a bare
///    `-boot start` lands on the installed `<Root>/bin/start.boot` (the installed location
///    of this checkout's `bootstrap/bin/start.boot`) and `init`'s own default `start`
///    resolves to the very same file.
///
/// `<Root>` is the Erlang root ([`boot_file_root`]); it is `None` for a runtime with no
/// system install on its code path, which is why a `$ROOT/…` value then fails rather than
/// silently resolving to something else.
///
/// A `$ROOT/…` value with no resolvable root, and a value whose two candidates both do not
/// exist, are a typed message naming the candidate — the oracle's
/// `exit({'cannot get bootfile', Name})` — never a silent skip and never a different file.
fn resolve_boot_file(value: &str, root: Option<&Path>) -> Result<PathBuf> {
    let candidate = match value.strip_prefix("$ROOT/") {
        Some(rest) => {
            let root = root.ok_or_else(|| {
                error::message(format!(
                    "cannot get bootfile {value}: `$ROOT` cannot be resolved — the code path \
                     has no <lib>/<app>-<vsn>/ebin directory"
                ))
            })?;
            root.join("bin").join(format!("{rest}.boot"))
        }
        None => PathBuf::from(format!("{value}.boot")),
    };
    if candidate.is_file() {
        return Ok(candidate);
    }
    if let Some(root) = root {
        // Deliberately a string concatenation, not `Path::join` (which would *replace* the
        // path for an absolute candidate): the oracle concatenates, so an absolute
        // `-boot /x/y` looks for `<Root>/bin//x/y.boot` and stays not-found there.
        let fallback = PathBuf::from(format!("{}/bin/{}", root.display(), candidate.display()));
        if fallback.is_file() {
            return Ok(fallback);
        }
    }
    Err(error::message(format!(
        "cannot get bootfile {}",
        candidate.display()
    )))
}

/// Resolves a boot-script **value** the way `init`'s `get_boot/2` does, against the runtime's
/// own Erlang root ([`boot_file_root`]).
///
/// This is the one seam shared by the CLI's `-boot FILE` action ([`run_boot_script`]) and the
/// runtime-native floor's `init:boot/1` (`../beam_rt/src/native/init.rs`) — one resolution
/// rule, so the two can never drift.
///
/// The runtime must be **booted** for `$ROOT` (and the `<Root>/bin` fallback) to resolve, since
/// `<Root>` is derived from the system `ebin` directories on the code path; a caller on a
/// floor path boots first.
///
/// # Errors
///
/// [`resolve_boot_file`]'s typed `cannot get bootfile …` error when a `$ROOT/…` value has no
/// resolvable root or both candidates are missing.
pub(crate) fn resolve_boot_file_for(runtime: &Runtime, value: &str) -> Result<PathBuf> {
    resolve_boot_file(value, boot_file_root(runtime).as_deref())
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
/// The export is driven through [`Runtime::run_scheduled`], which runs it as a real
/// scheduled process and pumps the **full** scheduler (processes, ports, timers,
/// distribution) until it terminates — so a workload that blocks (a driver/port echo
/// round-trip, say) is resumed by a later pass instead of dying on a single-pass
/// [`beam_engine::blocked`](beam_engine::blocked). A workload that never terminates is
/// reported honestly when [`Runtime::RUN_DEADLINE`] expires, and one blocked with nothing
/// able to wake it keeps the single-pass path's immediate answer (see that method).
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
    let result = match runtime.run_scheduled(file, function, arity, &terms) {
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
    use crate::config::MaxHeapFlags;

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
        // `-eval` is a Start instruction, not an init argument: the oracle's
        // `init:get_argument(eval)` is `error`.
        assert_eq!(config.init_args.get("eval"), None);
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
        // `-eval` is a Start instruction, so it is not an init argument.
        assert_eq!(parsed(&["-eval", "2+3."]).init_args.get("eval"), None);
    }

    #[test]
    fn code_path_switches_order_pa_before_pz() {
        // `-pa` is the **head** of the code path and `-pz` the **tail**, so the two groups
        // are kept apart (the ordering itself is applied by `CodeServer`):
        //   `erl -noshell -pa a1 a2 -pz z1 z2 -eval 'io:format("~p~n",[lists:reverse(code:get_path())]),halt().'`
        //   → the head is `["a2","a1", …]` (each `-pa` prepended in turn) and the tail is
        //     `["z2","z1", ".", <system ebins>]` — i.e. `-pz` lands *after* `"."`.
        // The parser therefore records `-pa` in `code_paths` and `-pz` in `pz_paths`.
        let config = parsed(&["-pa", "/a", "-pz", "/b", "-pa", "/c"]);
        assert_eq!(
            config.code_paths,
            vec![PathBuf::from("/a"), PathBuf::from("/c")]
        );
        assert_eq!(config.pz_paths, vec![PathBuf::from("/b")]);
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
        // A `+`-flag is an emulator flag, never an init argument.
        assert_eq!(config.init_args.get("S"), None);
        let config = parsed(&["+S", "8:4"]);
        assert_eq!(config.emulator.schedulers, Some(8));
        let config = parsed(&["+P", "1024"]);
        assert_eq!(config.emulator.max_processes, Some(1024));
        // `+P` is bounded by `[ERTS_MIN_PROCESSES=1024, ERTS_MAX_PROCESSES]`.
        assert!(parse_args(&["+P", "1000"]).is_err());
        let config = parsed(&["+A", "16"]);
        assert_eq!(
            config.emulator.extra_emulator_flags,
            vec!["+A".to_string(), "16".to_string()]
        );
        assert_eq!(config.emulator.schedulers, None);
        assert_eq!(config.init_args.get("A"), None);
        assert!(parse_args(&["+S", "x"]).is_err());
        assert!(parse_args(&["+S", "1:x"]).is_err());
        assert!(parse_args(&["+P", "-1"]).is_err());
        assert!(parse_args(&["+A", "many"]).is_err());
        assert!(parse_args(&["+S"]).is_err());
    }

    /// The `+hms`/`+hmbs` heap options, pinned against the oracle (OTP 29.1.1 /
    /// erts-17.1):
    ///
    /// ```text
    /// erl +hms 1000 +hmbs 5000 -noshell \
    ///     -eval 'io:format("~p~n",[process_info(self(),garbage_collection)]), halt().'
    ///   → min_bin_vheap_size 6772, min_heap_size 1598
    /// ```
    ///
    /// `+hms`/`+hmbs` are emulator flags: real `erl` does *not* report them
    /// (`erl +hms 1000 … -eval 'init:get_argument(hms)'` ⇒ `error`), and neither does
    /// this runtime — no `+`-flag is an init argument.
    #[test]
    fn heap_size_switches_are_parsed() {
        let config = parsed(&["+hms", "1000", "+hmbs", "5000"]);
        assert_eq!(config.heap_options().policy().min_heap_words, 1598);
        assert_eq!(config.heap_options().policy().min_bin_vheap_words, 6772);
        assert_eq!(config.init_args.get("hms"), None);
        assert_eq!(config.init_args.get("hmbs"), None);
        // A recognized heap switch is modelled, so it is *not* a verbatim extra flag
        // (unlike `+A`) — and, like every `+`-flag, it is not an init argument.
        assert!(config.emulator.extra_emulator_flags.is_empty());
        assert_eq!(config.heap_words, DEFAULT_HEAP_WORDS);
        assert_eq!(config.action, Action::Boot);

        // The last occurrence wins.
        let config = parsed(&["+hms", "100", "+hms", "234"]);
        assert_eq!(config.heap_options().policy().min_heap_words, 376);
        // `+hms 0`, a negative and a non-numeric value are all rejected — the emulator
        // rejects anything `<= 0` (`erl_init.c:1554-1567`) or non-numeric (`atoi`).
        for args in [
            &["+hms", "0"][..],
            &["+hms", "abc"][..],
            &["+hms", "-5"][..],
            &["+hms"][..],
            &["+hmbs", "0"][..],
            &["+hmbs", "abc"][..],
            &["+hmbs"][..],
        ] {
            let error = parse_args(args).expect_err("a malformed heap switch is a usage error");
            assert_eq!(error::unsupported_reason(&error), None, "{args:?}");
        }
    }

    /// The `+hmax*` family, pinned against the oracle (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl +hmax 1000000 -noshell \
    ///     -eval 'io:format("~p~n",[erlang:system_info(garbage_collection)]), halt().'
    ///   → [{max_heap_size,1000000},…]
    /// erl +hmax 1000000 +hmaxk false +hmaxel false +hmaxib true -noshell -eval …
    ///   → #{error_logger=>false,include_shared_binaries=>true,kill=>false,size=>1000}
    /// ```
    #[test]
    fn max_heap_switches_are_parsed() {
        let config = parsed(&[
            "+hmax",
            "1000000",
            "+hmaxk",
            "false",
            "+hmaxel",
            "false",
            "+hmaxib",
            "true",
        ]);
        assert_eq!(config.heap_options().policy().max_heap_words, 1_000_000);
        assert_eq!(
            config.heap_options().max_heap_flags(),
            MaxHeapFlags {
                kill: false,
                error_logger: false,
                include_shared_binaries: true,
            }
        );
        assert_eq!(
            config.init_args.get("hmax"),
            None,
            "a `+`-flag is never an init argument"
        );
        assert_eq!(config.init_args.get("hmaxk"), None);
        assert_eq!(config.init_args.get("hmaxib"), None);
        // `+hmax 0` is the emulator's own "no limit" value.
        assert_eq!(parsed(&["+hmax", "0"]).heap_options().policy().max_heap_words, 0);
        // A negative, a non-numeric or an out-of-range value is rejected (the C bounds it
        // by `0..=MAX_SMALL`, `erl_init.c:1626-1633`), as is a non-boolean flag value
        // (`erl_init.c:1592-1624`).
        for args in [
            &["+hmax", "-1"][..],
            &["+hmax", "abc"][..],
            &["+hmax", "1000000000000000000000"][..],
            &["+hmax"][..],
            &["+hmaxk", "bogus"][..],
            &["+hmaxel", "1"][..],
            &["+hmaxib"][..],
        ] {
            let error = parse_args(args).expect_err("a malformed max-heap switch is a usage error");
            assert_eq!(error::unsupported_reason(&error), None, "{args:?}");
        }
    }

    /// Unmodeled flags keep their behaviour when a modelled heap switch sits next to
    /// them: a `+`-flag is accepted (and, for `+A`, preserved verbatim) but is not an init
    /// argument, and parsing continues onto the next switch.
    #[test]
    fn unmodeled_flags_are_unaffected_by_the_heap_switches() {
        let config = parsed(&["+hms", "1000", "+zzz", "1", "2", "+A", "16"]);
        assert_eq!(config.heap_options().policy().min_heap_words, 1598);
        assert_eq!(config.init_args.get("zzz"), None);
        assert_eq!(
            config.emulator.extra_emulator_flags,
            vec!["+A".to_string(), "16".to_string()]
        );
        assert_eq!(config.init_args.get("hms"), None);
        // An unrecognized flag on its own still boots (accepted, not a usage error).
        assert_eq!(parsed(&["+Weird"]).heap_options(), &HeapOptions::default());
    }

    #[test]
    fn smp_switch_selects_the_scheduler_model() {
        // `disable` turns SMP off; it is still recorded as an init argument.
        let config = parsed(&["-smp", "disable"]);
        assert!(!config.emulator.features.smp, "disable turns SMP off");
        assert_eq!(
            config.init_args.get("smp"),
            Some(vec![&["disable".to_string()][..]])
        );
        // `enable`/`auto` are the default (SMP on).
        assert!(parsed(&["-smp", "enable"]).emulator.features.smp);
        assert!(parsed(&["-smp", "auto"]).emulator.features.smp);
        // A value the runtime does not model is accepted and recorded but changes nothing
        // (the oracle's leniency: `erl -noshell -smp bogus -eval 'halt().'` exits 0).
        let config = parsed(&["-smp", "bogus"]);
        assert!(config.emulator.features.smp, "an unknown value is a no-op");
        assert_eq!(
            config.init_args.get("smp"),
            Some(vec![&["bogus".to_string()][..]])
        );
        // A valueless `-smp` is accepted too (recorded with no values).
        let config = parsed(&["-smp"]);
        assert!(config.emulator.features.smp);
        assert_eq!(config.init_args.get("smp"), Some(vec![&[][..]]));
        // `-smp` consumes only its value: the following switch still parses.
        let config = parsed(&["-smp", "disable", "+S", "2"]);
        assert!(!config.emulator.features.smp);
        assert_eq!(config.emulator.schedulers, Some(2));
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
        // `-s` is a Start instruction, not an init argument.
        assert_eq!(config.init_args.get("s"), None);

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
        // `-run` is a Start instruction, not an init argument.
        assert_eq!(config.init_args.get("run"), None);
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

        // A `+`-flag is an emulator flag: accepted, but invisible to
        // `init:get_argument/1` (the oracle reports `error`).
        let config = parsed(&["+Weird"]);
        assert_eq!(config.init_args.get("Weird"), None);

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

    /// The A.6 configuration switches (`-setcookie`/`-config`/`-boot`): a `-`-switch, so
    /// each is an **init argument**, exactly as the oracle
    /// (`erl -setcookie abc -noshell -eval 'io:format("~w~n",[init:get_argument(setcookie)]), halt().'`
    /// ⇒ `{ok,[["abc"]]}`; verified against OTP 29.1.1 / erts-17.1).
    #[test]
    fn the_configuration_switches_are_recorded_as_init_arguments() {
        let config = parsed(&["-setcookie", "abc"]);
        assert_eq!(config.emulator.cookie.as_deref(), Some("abc"));
        assert_eq!(
            config.init_args.get("setcookie"),
            Some(vec![&["abc".to_string()][..]])
        );
        // Every occurrence is recorded (`{ok,[["one"],["two"]]}`); the modelled cookie is
        // the last one.
        let config = parsed(&["-setcookie", "one", "-setcookie", "two"]);
        assert_eq!(config.emulator.cookie.as_deref(), Some("two"));
        assert_eq!(
            config.init_args.get("setcookie"),
            Some(vec![&["one".to_string()][..], &["two".to_string()][..]])
        );

        let config = parsed(&["-config", "/etc/app.config"]);
        assert_eq!(
            config.init_args.get("config"),
            Some(vec![&["/etc/app.config".to_string()][..]])
        );
        let config = parsed(&["-boot", "/boot/start"]);
        assert_eq!(
            config.init_args.get("boot"),
            Some(vec![&["/boot/start".to_string()][..]])
        );

        // A missing operand of any of the three is a usage error, never a capability gap.
        for switch in ["-setcookie", "-config", "-boot"] {
            let error = parse_args(&[switch]).expect_err("a missing operand");
            assert_eq!(error::unsupported_reason(&error), None, "{switch}");
        }
    }

    /// `-emu_flavor F` records the value as an init argument and selects the flavor:
    /// `emu` ⇒ [`beam_common::Flavor::Emu`], `jit`/`smp` ⇒ [`beam_common::Flavor::Jit`].
    ///
    /// Divergence: the oracle's `erlexec` accepts only `smp` (`-emu_flavor emu` ⇒
    /// `erlexec: Invalid emulator type or flavor.`, exit 1); accepting `emu`/`jit` is what
    /// makes [`beam_common::Flavor::Emu`] reachable here. For `smp` the recorded value is
    /// the same (`{ok,[["smp"]]}`).
    #[test]
    fn emu_flavor_switch_selects_the_flavor_and_is_recorded() {
        let config = parsed(&["-emu_flavor", "emu"]);
        assert_eq!(config.emulator.flavor, beam_common::Flavor::Emu);
        assert_eq!(
            config.emulator.features,
            beam_common::FeatureFlags::for_flavor(beam_common::Flavor::Emu)
        );
        assert_eq!(
            config.init_args.get("emu_flavor"),
            Some(vec![&["emu".to_string()][..]])
        );
        for value in ["jit", "smp"] {
            let config = parsed(&["-emu_flavor", value]);
            assert_eq!(config.emulator.flavor, beam_common::Flavor::Jit);
            assert_eq!(
                config.emulator.features,
                beam_common::FeatureFlags::for_flavor(beam_common::Flavor::Jit)
            );
            assert_eq!(
                config.init_args.get("emu_flavor"),
                Some(vec![&[value.to_string()][..]])
            );
        }
        for args in [&["-emu_flavor", "bogus"][..], &["-emu_flavor"][..]] {
            let error = parse_args(args).expect_err("a bad/missing flavor is a usage error");
            assert_eq!(error::unsupported_reason(&error), None, "{args:?}");
        }
    }

    /// The `+Q`/`+P`/`+K`/`+sbt` emulator switches: validated against the legacy C, and
    /// **never** init arguments (in the oracle every `+`-flag is an emulator flag, so
    /// `init:get_argument('Q'|'P'|'K'|'sbt')` is the bare atom `error`).
    ///
    /// Oracle (OTP 29.1.1 / erts-17.1): `+Q`/`+P` accept `legacy`/`1024`/`134217727`
    /// (exit 0) and reject `1000`/`0`/`134217728`/`abc` (exit 1); all nine `+sbt` types
    /// exit 0 and `+sbt bogus` exits 1; `+K bogus` exits 0 (the emulator does not
    /// validate it — `erl_check_io.c:2467`).
    #[test]
    fn limit_switches_are_validated_preserved_and_never_init_arguments() {
        // `+Q` — preserved verbatim *and* modelled as the port-table request the runtime
        // reports through `system_info(port_limit)`.
        for value in ["1024", "134217727"] {
            let config = parsed(&["+Q", value]);
            assert_eq!(
                config.emulator.extra_emulator_flags,
                vec!["+Q".to_string(), value.to_string()]
            );
            assert_eq!(
                config.max_ports,
                Some(value.parse::<usize>().expect("a decimal switch value")),
                "+Q {value}"
            );
            assert_eq!(config.init_args.get("Q"), None);
        }
        // `+Q legacy` selects the emulator's default table: no request at all.
        let config = parsed(&["+Q", "legacy"]);
        assert_eq!(
            config.emulator.extra_emulator_flags,
            vec!["+Q".to_string(), "legacy".to_string()]
        );
        assert_eq!(config.max_ports, None);
        // Without `+Q` there is no request either.
        assert_eq!(parsed(&[]).max_ports, None);
        for value in ["1000", "0", "1023", "134217728", "abc", "1024abc", ""] {
            let error = parse_args(&["+Q", value]).expect_err("a bad +Q value");
            assert_eq!(error::unsupported_reason(&error), None, "{value:?}");
        }
        assert!(parse_args(&["+Q"]).is_err(), "+Q needs a value");

        // `+P` — modelled as the emulator's process limit; `legacy` keeps the default.
        let config = parsed(&["+P", "1024"]);
        assert_eq!(config.emulator.max_processes, Some(1024));
        assert_eq!(config.init_args.get("P"), None);
        assert!(config.emulator.extra_emulator_flags.is_empty());
        assert_eq!(parsed(&["+P", "legacy"]).emulator.max_processes, None);
        assert_eq!(
            parsed(&["+P", "134217727"]).emulator.max_processes,
            Some(134_217_727)
        );
        for value in ["1000", "0", "134217728", "abc", ""] {
            let error = parse_args(&["+P", value]).expect_err("a bad +P value");
            assert_eq!(error::unsupported_reason(&error), None, "{value:?}");
        }
        assert!(parse_args(&["+P"]).is_err(), "+P needs a value");

        // `+K` — preserved; the oracle accepts *any* value here, so validating the two
        // literals is a documented divergence (`erl +K bogus …` exits 0).
        for value in ["true", "false"] {
            let config = parsed(&["+K", value]);
            assert_eq!(
                config.emulator.extra_emulator_flags,
                vec!["+K".to_string(), value.to_string()]
            );
            assert_eq!(config.init_args.get("K"), None);
        }
        let error = parse_args(&["+K", "bogus"]).expect_err("beam_rt validates the literals");
        assert_eq!(error::unsupported_reason(&error), None);
        assert!(parse_args(&["+K"]).is_err(), "+K needs a value");

        // `+sbt` — the nine bind types of `erl_cpu_topology.c:628-647`.
        for value in ["u", "db", "s", "ps", "ts", "tnnps", "nnps", "nnts", "ns"] {
            let config = parsed(&["+sbt", value]);
            assert_eq!(
                config.emulator.extra_emulator_flags,
                vec!["+sbt".to_string(), value.to_string()]
            );
            assert_eq!(config.init_args.get("sbt"), None);
        }
        for args in [&["+sbt", "bogus"][..], &["+sbt"][..]] {
            let error = parse_args(args).expect_err("a bad/missing +sbt value");
            assert_eq!(error::unsupported_reason(&error), None, "{args:?}");
        }
    }

    /// The `-M*`/`+M*` allocator family: recognized, **never** an init argument; `-M*` is
    /// accepted unvalidated (oracle parity — `erlexec` never forwards it to the emulator)
    /// while `+M*` is validated against the `erl_alloc.c` grammar.
    #[test]
    fn allocator_switches_are_accepted_and_never_init_arguments() {
        // The validated `+M*` forms (in-token and next-token values both work).
        for args in [
            &["+MMscs", "1024"][..],
            &["+MMscs1024"][..],
            &["+MBas", "aobf"][..],
            &["+MBasbf"][..],
            &["+Mlpm", "no"][..],
            &["+MYe", "true"][..],
            &["+MIscs", "100"][..],
            &["+MMamcbf", "100"][..],
            &["+MZas", "chaosff"][..],
        ] {
            let config = parsed(args);
            assert_eq!(config.action, Action::Boot, "{args:?}");
            assert!(
                config.init_args.is_empty(),
                "{args:?} must record no init argument"
            );
        }

        // A **`-M*`** flag is not an emulator flag at all: `erlexec` never rewrites it, so
        // the emulator sees an ordinary unknown `-flag` and `init` records it as an init
        // argument (name = the token minus the leading `-`, values = the following
        // non-switch tokens). Verified against OTP 29.1.1 / erts-17.1:
        // `erl -noshell -Mfoo 1 …` ⇒ `init:get_argument('Mfoo')` = `{ok,[["1"]]}`.
        for (args, name, values) in [
            (&["-Mfoo"][..], "Mfoo", Vec::<String>::new()),
            (&["-M"][..], "M", Vec::new()),
            (&["-Mea"][..], "Mea", Vec::new()),
            (&["-Mfoo", "1"][..], "Mfoo", vec!["1".to_string()]),
            (&["-Mea", "min"][..], "Mea", vec!["min".to_string()]),
            (&["-Mea", "1024"][..], "Mea", vec!["1024".to_string()]),
        ] {
            let config = parsed(args);
            assert_eq!(config.action, Action::Boot, "{args:?}");
            assert_eq!(
                config.init_args.get(name),
                Some(vec![values.as_slice()]),
                "{args:?}"
            );
        }

        // `+M*`, by contrast, *is* an emulator flag and its value is validated (the oracle
        // rejects all of these with exit 1; `beam_rt` reports its uniform usage error).
        for args in [
            &["+Mfoo"][..],
            &["+M"][..],
            &["+MMscs", "bogus"][..],
            &["+MBas", "chaosff"][..],
        ] {
            let error = parse_args(args).expect_err("a bad +M* value");
            assert_eq!(error::unsupported_reason(&error), None, "{args:?}");
        }

        // `+Mea <preset>` is the emulator-wide allocator preset the oracle accepts
        // (`erl_alloc.c:1733-1783`; `+Mea min` exits 0). Like every `+`-flag it is
        // validated, consumes its value, and records no init argument.
        let config = parsed(&["+Mea", "min"]);
        assert_eq!(config.action, Action::Boot);
        assert!(
            config.init_args.is_empty(),
            "+Mea min records no init argument"
        );
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
            "+Q",
            "+K",
            "+sbt",
            "-MNAME",
            "+MM",
            "+MB",
            "-name",
            "-setcookie",
            "-config",
            "-boot",
            "-emu_flavor",
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

    /// A scratch directory the boot-file resolution tests write their candidates into,
    /// removed on drop.
    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        /// Creates an empty scratch directory unique to `label` and this process.
        fn new(label: &str) -> ScratchDir {
            let path = std::env::temp_dir().join(format!(
                "beam_rt_cli_boot_{label}_{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path)
                .unwrap_or_else(|error| panic!("cannot create {}: {error}", path.display()));
            ScratchDir { path }
        }

        /// Writes `name` (relative to the scratch directory, parents created) and returns
        /// its full path.
        fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
            let file = self.path.join(name);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|error| {
                    panic!("cannot create {}: {error}", parent.display())
                });
            }
            std::fs::write(&file, contents)
                .unwrap_or_else(|error| panic!("cannot write {}: {error}", file.display()));
            file
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                eprintln!("note: cannot remove {}: {error}", self.path.display());
            }
        }
    }

    /// The `-boot FILE` wiring: the switch stays the `boot` **init argument** the oracle
    /// reports *and* becomes the script value the boot drive resolves. The boot action is
    /// the only action that carries it (a start instruction keeps the preloaded-only
    /// floor), so the extraction is pinned per action.
    #[test]
    fn boot_switch_is_recorded_and_selected_for_the_boot_action() {
        let config = parsed(&["-noshell", "-boot", "start"]);
        assert_eq!(config.action, Action::Boot);
        assert_eq!(
            config.init_args.get("boot"),
            Some(vec![&["start".to_string()][..]])
        );
        assert_eq!(boot_file_argument(&config), Some("start"));

        // The **first** occurrence wins — the oracle's `lists:keyfind/3`.
        let config = parsed(&["-boot", "first", "-boot", "second"]);
        assert_eq!(boot_file_argument(&config), Some("first"));

        // No `-boot`: the boot action keeps its preloaded-only drive.
        assert_eq!(boot_file_argument(&parsed(&["-noshell"])), None);
        assert_eq!(boot_file_argument(&parsed(&[])), None);
        // The **flag** is action-independent, but only the boot action drives it: a start
        // instruction keeps the preloaded-only floor (it runs code, not the boot script).
        let config = parsed(&["-boot", "x", "-eval", "2+3"]);
        assert_eq!(boot_file_argument(&config), Some("x"));
        assert_eq!(config.action, Action::Eval("2+3".to_string()));
        // `-config` is a different flag, never a boot script.
        assert_eq!(boot_file_argument(&parsed(&["-config", "/etc/app.config"])), None);

        // A `-boot` with no operand is a usage error, never a silent default.
        assert!(parse_args(&["-boot"]).is_err());
    }

    /// The boot-file resolution mirrors `init`'s `get_boot/2` (`erts/preloaded/src/init.erl`):
    /// the candidate is `<Name>.boot` (or `<Root>/bin/<Name>.boot` for a `$ROOT/…` value) and
    /// the fallback is the **string** `<Root>/bin/<candidate>`, so `-boot start` finds the
    /// installed `<Root>/bin/start.boot` and a value whose candidates are both absent is a
    /// typed error naming the candidate.
    #[test]
    fn the_boot_file_is_resolved_exactly_as_init_does() {
        let scratch = ScratchDir::new("resolve");
        let root = scratch.path.join("root");
        // A name no directory of this test run can hold, so the "relative to the current
        // directory" candidate is provably absent and the `<Root>/bin` fallback is what
        // answers (exactly the oracle's default `start` resolution).
        let name = format!("beam_rt_cli_{}", std::process::id());
        let installed = scratch.write(&format!("root/bin/{name}.boot"), b"installed");

        // (1)+(2) a bare name: `<Name>.boot` relative to the current directory misses, so
        // the `<Root>/bin` fallback wins.
        assert_eq!(
            resolve_boot_file(&name, Some(&root)).expect("the <Root>/bin fallback"),
            installed
        );
        // (1) the `$ROOT/<Name>` form is `<Root>/bin/<Name>.boot`, hit directly.
        assert_eq!(
            resolve_boot_file(&format!("$ROOT/{name}"), Some(&root)).expect("the $ROOT form"),
            installed
        );
        // (1) an existing path is taken verbatim with `.boot` appended.
        let explicit = scratch.write("mine.boot", b"explicit");
        let value = scratch.path.join("mine");
        assert_eq!(
            resolve_boot_file(&value.to_string_lossy(), Some(&root)).expect("an explicit file"),
            explicit
        );
        // `.boot` is appended unconditionally, as the oracle does: `-boot …/mine.boot` looks
        // for `…/mine.boot.boot`.
        let with_suffix = scratch.path.join("mine.boot");
        assert!(
            resolve_boot_file(&with_suffix.to_string_lossy(), Some(&root)).is_err(),
            "the `.boot` suffix is appended, never inferred from the argument"
        );

        // Both candidates absent: a typed message naming the candidate, never a silent skip
        // and never a different file. The fallback is a *string* concatenation, so an
        // absolute candidate lands under `<Root>/bin` and stays not-found.
        let absent = scratch.path.join("absent");
        let error = resolve_boot_file(&absent.to_string_lossy(), Some(&root))
            .expect_err("an absent boot file is an error");
        let text = format!("{error}");
        assert!(text.contains("cannot get bootfile"), "{text}");
        assert!(text.contains("absent.boot"), "{text}");

        // A `$ROOT/…` value with no resolvable root is refused rather than guessed at.
        let error = resolve_boot_file("$ROOT/start", None)
            .expect_err("an unresolvable $ROOT is an error");
        assert!(format!("{error}").contains("cannot get bootfile"), "{error}");
    }

    /// The `Boot` action really drives the executor: an unknown `-boot` value reaches the
    /// resolution and is reported as a runtime error (exit 1), never a usage error, a panic
    /// or a silent success.
    #[test]
    fn a_boot_flag_naming_an_absent_script_is_a_runtime_error() {
        assert_eq!(
            run(vec!["-boot".to_string(), "/nonexistent/nope".to_string()]),
            EXIT_RUNTIME
        );
    }

    /// `-config FILE` is resolved exactly as the application controller resolves it:
    /// `filename:basename(FileName, ".config")` strips **one** trailing `.config` and the
    /// name is rebuilt in its own directory (`lib/kernel/src/application_controller.erl:1876-1879`).
    #[test]
    fn the_config_file_name_is_resolved_the_way_the_oracle_resolves_it() {
        for (value, resolved) in [
            ("/x/conf", "/x/conf.config"),
            ("/x/conf.config", "/x/conf.config"),
            ("conf", "conf.config"),
            ("conf.config", "conf.config"),
            // The strip is on the **basename** only, so a dotted directory survives.
            ("/a.b/conf", "/a.b/conf.config"),
            // At most one occurrence is stripped and re-appended.
            ("/x/y.config.config", "/x/y.config.config"),
        ] {
            assert_eq!(resolve_config_file(value), PathBuf::from(resolved), "{value}");
        }
    }

    /// The boot-time `-config` check. Oracle (OTP 29.1.1 / erts-17.1), all verified:
    ///
    /// ```text
    /// erl -noshell -config /nonexistent      -eval 'ok'  → exit 1  (no stdout, no stderr)
    /// erl -noshell -config <dir named>.config -eval 'ok' → exit 1
    /// erl -noshell -config <missing> -config <ok> -eval  → exit 1
    /// erl -noshell -config <ok> -config <missing> -eval  → exit 1
    /// erl -noshell -config <existing>.config -eval 'ok'  → exit 0
    /// ```
    #[test]
    fn a_config_file_that_cannot_be_read_is_refused() {
        let scratch = ScratchDir::new("config");
        let present = scratch.write("app.config", b"[].\n");

        // No `-config` at all: nothing to check.
        assert!(check_config_file(&parsed(&["-noshell"])).is_ok());

        // Both spellings of the same file are accepted (`.config` is appended unless present).
        let stem = present.with_extension("");
        for value in [present.to_string_lossy(), stem.to_string_lossy()] {
            let config = parsed(&["-config", value.as_ref()]);
            assert!(check_config_file(&config).is_ok(), "{value}");
        }

        // A missing file is refused with a message naming the **resolved** path.
        let absent = scratch.path.join("absent");
        let error = check_config_file(&parsed(&["-config", absent.to_string_lossy().as_ref()]))
            .expect_err("a missing config file is refused");
        let text = format!("{error}");
        assert!(text.contains("cannot read config file"), "{text}");
        assert!(text.contains("absent.config"), "{text}");

        // A directory carrying the suffix is not a file (the oracle's `file:consult`
        // reports `eisdir` out of the boot).
        std::fs::create_dir_all(scratch.path.join("dir.config")).expect("a directory");
        let directory = scratch.path.join("dir");
        assert!(
            check_config_file(&parsed(&["-config", directory.to_string_lossy().as_ref()])).is_err(),
            "a directory is not a readable config file"
        );

        // **Every** occurrence is checked, not just the first — the oracle refuses either
        // order when one of the two cannot be read.
        let present = present.to_string_lossy().into_owned();
        let absent = absent.to_string_lossy().into_owned();
        for (first, second) in [(&absent, &present), (&present, &absent)] {
            let config = parsed(&["-config", first, "-config", second]);
            assert!(
                check_config_file(&config).is_err(),
                "-config {first} -config {second} must be refused"
            );
        }
        // … and two readable ones are accepted.
        let config = parsed(&["-config", &present, "-config", &present]);
        assert!(check_config_file(&config).is_ok());
    }

    /// The end-to-end classification: a `-config` the boot cannot read is a **runtime** error
    /// (exit 1 + `beam_rt: <error>`), never a usage error (2) and never a silent success —
    /// while `-version`, which builds no runtime at all, stays exempt (the oracle's own
    /// `erl -version -config /nonexistent` exits 0 with the banner).
    #[test]
    fn a_missing_config_file_exits_1_but_version_stays_exempt() {
        for action in [
            vec!["-noshell", "-config", "/nonexistent/nope", "-eval", "ok"],
            vec!["-noshell", "-config", "/nonexistent/nope"],
            vec!["-noshell", "-config", "/nonexistent/nope", "-s", "init", "stop"],
        ] {
            let args: Vec<String> = action.iter().map(|arg| arg.to_string()).collect();
            assert_eq!(run(args), EXIT_RUNTIME, "{action:?}");
        }
        assert_eq!(
            run(vec![
                "-version".to_string(),
                "-config".to_string(),
                "/nonexistent/nope".to_string(),
            ]),
            EXIT_OK
        );
    }
}