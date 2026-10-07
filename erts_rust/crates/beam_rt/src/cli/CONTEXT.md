# `beam_rt::cli` — the A.6 command-line surface (switch parser · `ERL_*FLAGS` merge · emulator-flag validators)

## Intent
The command-line surface of the `beam_rt` executable (contract A.6).
The parser and driver live in the sibling file `../cli.rs`; this directory holds the two concerns split out of it: the `ERL_*FLAGS` environment merge (`env.rs`) and the validators for the emulator switches that have no runtime-visible effect (`flags.rs`).
All three share two goals: a tokenized environment flag must be indistinguishable from a command-line flag, and the init-argument set must match the oracle's `init:get_argument/1`.

## API Surface
`../cli.rs` (re-exported at `beam_rt::cli`):
* `parse(args: &[String]) -> Result<RuntimeConfig>` — **pure**, never reads the process environment (the unit tests, the driver and the differential harness call it directly).
* `parse_with_env(args: &[String], env: &dyn Env) -> Result<RuntimeConfig>` — merges the environment, then calls the pure `parse`.
* `run(args: Vec<String>) -> i32` — the driver; it calls `parse_with_env(&args, &ProcessEnv)`.
* `USAGE` (the usage text) and `EXIT_OK`/`EXIT_RUNTIME`/`EXIT_USAGE`/`EXIT_UNSUPPORTED` (= 0/1/2/3).
* `merge_argv`, `tokenize`, `otp_flags_var`, `Env`, `MapEnv`, `ProcessEnv` — re-exported from `env`.
`env.rs`:
* `trait Env { fn get(&self, name: &str) -> Option<String> }` — the environment the merge reads from.
* `ProcessEnv` — the production `Env` over `std::env::var`; a variable that is set but not valid UTF-8 is folded into "unset".
* `MapEnv(HashMap<String, String>)` — the fixed, unit-testable `Env`.
* `tokenize(&str) -> Vec<String>` — the `build_args_from_string` (`allow_comments = 0`) tokenizer.
* `merge_argv(args, env) -> Vec<String>` — the `initial_argv_massage` merge.
* `otp_flags_var() -> String` — the `ERL_OTP<sysver>_FLAGS` variable name (`ERL_OTP30_FLAGS`).
`flags.rs` (all `pub(super)`, consumed by `../cli.rs`):
* `is_allocator_switch` / `parse_allocator` — the `-M*`/`+M*` family.
* `parse_max_ports` (`+Q`), `parse_max_processes` (`+P`), `parse_kernel_poll` (`+K`), `parse_sbt` (`+sbt`), `parse_emu_flavor` (`-emu_flavor`).
* the ground-truth constants `ERTS_MIN_PORTS`/`ERTS_MAX_PORTS`/`ERTS_MIN_PROCESSES`/`ERTS_MAX_PROCESSES` and `SCHEDULER_BIND_TYPES`.
`vars.rs` (the `ERL_*` variables **outside** the `*FLAGS` merge; see the section below):
* `ErlEnv` — `{ inetrc, max_ports, crash_dump, crash_dump_seconds, rootdir, max_ets_tables, fullsweep_after }`, every field `Option` (`None` = unset).
* `from_env(&dyn Env) -> ErlEnv` — re-exported at `beam_rt::cli` (`pub use vars::{from_env, ErlEnv}`).
* `has_plus_q_switch(&[String]) -> bool` (`pub(super)`) — the `+Q`-wins-over-`ERL_MAX_PORTS` test.
* per-variable private parsers (`parse_max_ports`, `parse_atoi_var`, `c_atoi`) so each is unit-testable alone.

## The supported switches (the exact A.6 surface)
| Switch | Effect in `beam_rt` |
|---|---|
| `-version`, `--version` | print the version banner (OTP `30.0-rc0` / erts `17.1`) and exit 0 |
| `-noshell` | run head-less (M1 has no interactive shell) |
| `-eval Expr` | evaluate a general Erlang expression and print nothing; one trailing `.` is tolerated and stripped |
| `-s Mod Func [Args…]` | apply `Mod:Func(a, b, …)` with **atom** arguments; result discarded |
| `-run Mod Func [Args…]` | same, but arguments are plain **strings** (char lists); result discarded |
| `-pa Dir` / `-pz Dir` | add `Dir` to the code path (searched before / after `-pz` dirs) |
| `+S N[:N]` | normal scheduler count → `emulator.schedulers` (the first number) |
| `-smp enable\|disable\|auto` | scheduler model; only `disable` changes behaviour |
| `-name N` / `-sname N` | node name → `emulator.node_name`; makes the runtime a distributed node (A.5 phase 1) |
| `-setcookie C` | distribution cookie → `emulator.cookie` (also the init argument `setcookie`) |
| `-config F` | config file, recorded as the init argument `config` |
| `-boot F` | boot script, recorded as the init argument `boot` |
| `-emu_flavor emu\|jit\|smp` | emulator flavor → `emulator.flavor` + `FeatureFlags` |
| `+A N` | async-thread pool size, preserved verbatim |
| `+P N` | max process count (`legacy`, or `1024..=134217727`) → `emulator.max_processes` |
| `+Q N` | max port count (`legacy`, or `1024..=134217727`), preserved verbatim |
| `+K true\|false` | kernel-poll switch, preserved verbatim |
| `+sbt TYPE` | scheduler bind type (`u`/`db`/`s`/`ps`/`ts`/`tnnps`/`nnps`/`nnts`/`ns`), preserved verbatim |
| `-M*` / `+MM*` / `+MB*` | ERTS allocator flags; `+M*` is validated, `-M*` accepted unvalidated; all preserved verbatim |
| `+hms N` / `+hmbs N` / `+hmax N` / `+hmaxk\|el\|ib B` | GC heap policy (`beam_gc::HeapPolicy`), rounded through the growth table; visible in `garbage_collection` |
| `run <file.beam> <mod>:<func>/<arity> [arg…]` | the vertical slice: load the file, run, print the result |
An unmodeled `-FLAG`/`+FLAG` is **data, not a usage error** (it is preserved, and a `-`-flag is recorded); only a **stray positional argument** (outside `run`) is a usage error (exit 2).

## `ERL_*FLAGS` — the environment merge
`parse_with_env` merges four environment variables into the command line before parsing, in real `erl`'s order (`erts/etc/common/erlexec.c`, `initial_argv_massage`):
`[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]`.
So `ERL_AFLAGS` is **prepended** before the command line and `ERL_FLAGS` then `ERL_ZFLAGS` are **appended** after it, the command line keeping the middle position.
`<sysver>` is the OTP **system version** — the major of `OTP_VERSION` — so this runtime's variable is `ERL_OTP30_FLAGS` (`otp_flags_var`).
An **unset** variable contributes nothing; every set variable is `tokenize`d, so one variable can expand to several switches (and a whitespace-only value to none).
A merged token is then indistinguishable from a command-line flag to `init:get_argument/1`.

### Tokenizer rules (`build_args_from_string`, `allow_comments = 0`)
* Runs of the six ASCII whitespace bytes (space, `\t`, `\n`, `\r`, `\x0b`, `\x0c`) separate tokens — **not** Unicode whitespace.
* A `'…'` or `"…"` group is **one** token with the quotes removed; the matching quote ends the group, and quotes may open mid-token (`ab"c d"ef` → the token `abc def`, `"a b"'c d'` → `a bc d`).
* `\` escapes (literalizes) the **next** character, including whitespace and a quote (`a\ b` → the token `a b`).
* `#` is an ordinary character — it is **not** a comment.
* An empty string yields no tokens; a quoted empty group (`""`) yields one empty token.

## Environment variables beyond the `*FLAGS` chain (`vars.rs`)
A **second** family is read outside `initial_argv_massage` — by the emulator's start-up `getenv`s (`erts/emulator/beam/erl_init.c:1327-1344`, `erl_crash_dump.c:427-457`), by `inet_config` (`lib/kernel/src/inet_config.erl:436`) and by the `erl` **launcher script** (`ERL_ROOTDIR`).
`vars::from_env` parses them into `RuntimeConfig::env` (`config.env_vars()`); **none is an init argument** — verified against the oracle (OTP 29.1.1 / erts-17.1), where `init:get_argument(inetrc|crash_dump|crash_dump_seconds|max_ports|max_ets_tables|fullsweep_after)` is the bare atom `error`, so none is ever recorded in `InitArgs`.

| Variable | Oracle effect | `beam_rt` handling | Observable? |
|---|---|---|---|
| `ERL_MAX_PORTS` | `atoi`, then **clamped** into `[1024, 134217727]` (`erl_init.c:1341`); sizes the port table | `ErlEnv::max_ports` (the clamped `usize`) **and** injected into `extra_emulator_flags` as `+Q <clamped>` when the merged argv has no `+Q` | via `+Q` in `extra_emulator_flags`; the runtime's own `system_info` has no `port_limit` key yet (seam below) |
| `ERL_INETRC` | path the `inet` resolver reads at start-up | `ErlEnv::inetrc` (verbatim; `Some("")` when set-but-empty) | no — parsed-but-inert (M1 has no resolver) |
| `ERL_CRASH_DUMP` | crash-dump file name (emulator default `erl_crash.dump`) | `ErlEnv::crash_dump` (verbatim) | no — parsed-but-inert (no crash-dump subsystem) |
| `ERL_CRASH_DUMP_SECONDS` | `atoi` (`erl_crash_dump.c:451-457`): `0` = no dump + break now, `< 0` = dump until complete, `> 0` = alarm-bounded | `ErlEnv::crash_dump_seconds` (`i64`; a malformed value is `atoi`'s `0`) | no — parsed-but-inert |
| `ERL_ROOTDIR` | OTP-root relocation honoured by the `erl` **launcher script** | `ErlEnv::rootdir` (verbatim) | no — parsed-but-inert (this binary is not relocatable) |
| `ERL_MAX_ETS_TABLES` | `atoi` (`erl_init.c:1328-1331`); `0` = the emulator default | `ErlEnv::max_ets_tables` (`i64`) | no — parsed-but-inert (no ETS table limit here) |
| `ERL_FULLSWEEP_AFTER` | `atoi` (`erl_init.c:1334-1338`); the emulator stores it in a `Uint16` gen-GC interval | `ErlEnv::fullsweep_after` (`i64`, stored **untruncated** — no gen-GC consumer exists) | no — parsed-but-inert |

**Numeric semantics** are the C's `atoi` (`vars::c_atoi`): leading whitespace, an optional sign, then digits, stopping at the first non-digit — so a non-numeric value is `0`, never a dropped variable and never a wrong value.
`ERL_MAX_PORTS` is the one member with an effect, and it is reproduced through the channel `+Q` already uses:

* the clamp is what the emulator does to the value (`1`/`100`/`bogus` ⇒ 1024, `999999999` ⇒ 134217727); the `2000 ⇒ 2048` step the oracle **observes** in `system_info(port_limit)` is the port table's power-of-two sizing, an emulator concern this parser deliberately does not model;
* a `+Q` **anywhere in the merged argv** — the command line or `ERL_AFLAGS`/`ERL_FLAGS`/`ERL_ZFLAGS` — overrides the variable outright (oracle: `ERL_MAX_PORTS=2048 erl +Q 4096` and `ERL_FLAGS='+Q 4096' erl` both observe 4096), so `ERL_MAX_PORTS` is injected **only** when no `+Q` is present;
* the asymmetry is the oracle's: `+Q` **validates** (and rejects out-of-range), `ERL_MAX_PORTS` **clamps silently**.
`parse` stays pure — it never reads an environment, so it always yields `ErlEnv::default()`; only `parse_with_env` fills `env_vars` (and performs the `+Q` routing).
**Seam (sibling-owned):** the value becomes *observable* only once `../info.rs` gains a `port_limit` `system_info` key; until then it is preserved in `extra_emulator_flags`, not applied.

## Init arguments (`init:get_argument/1`) — the exact recording rule
The parser reproduces the oracle's split between emulator/`init` switches and flags:
* A **`-`-switch** (recognized or not) is recorded as an `InitArgs` occurrence — the source of `init:get_argument/1`.
* A **`+`-flag** is an **emulator** flag and is **not** recorded (the oracle's `init:get_argument('S'|'Q'|'K'|'A'|'P'|'sbt')` is the bare atom `error`).
* The `-eval`/`-s`/`-run` **Start instructions** are consumed by `init` and are **not** recorded (`init:get_argument('eval'|'s'|'run')` is the bare atom `error`).
A flag's name is its switch with the leading `-`/`+` run removed (`--nope` → `nope`, `-pa` → `pa`).
A valueless flag is one occurrence with no values (`-noshell` ⇒ `{ok,[[]]}`); `-pa /a -pa /b` is two occurrences (`{ok,[["/a"],["/b"]]}`).
The oracle additionally reports its own `root`/`bindir`/`progname`/`home` entries, which this runtime does not model.

## Exit codes (the stable external contract)
| Code | Meaning | stderr |
|---|---|---|
| `0` (`EXIT_OK`) | success | — |
| `1` (`EXIT_RUNTIME`) | genuine runtime error (a load/execute/BIF failure, e.g. a missing `.beam`, an `undef`, BIF `system_limit`) | `beam_rt: <error>` |
| `2` (`EXIT_USAGE`) | malformed invocation / usage error (a missing operand of a recognized switch, a malformed value, a malformed `run` spec/argument, a stray positional argument) | `beam_rt: <error>` then `USAGE` |
| `3` (`EXIT_UNSUPPORTED`) | **unsupported** — a Milestone-1 capability gap | exactly one line: `beam_rt: unsupported: <reason>` (no `USAGE`) |
A malformed `-eval` expression is **not** decided by the parser — it is a capability gap detected when the expression is evaluated (exit 3).
A runtime **halt** the program requests is neither: it exits with the requested status printing nothing.

## Documented divergences
* **Malformed emulator value: exit 2 vs the oracle's 1.** The oracle prints `bad …` + its usage text and exits **1**; `beam_rt` keeps its uniform four-code contract and reports a **usage** error (exit 2 + `USAGE`).
* **No `ErtsStrToSint` trailing-garbage laxness.** The C's numeric parse silently ignores trailing text (`+Q 1024abc` ⇒ 1024, `+hms 100abc` ⇒ 100); `beam_rt` rejects it.
* **`-emu_flavor` is broader than `erlexec`.** The oracle's `erlexec` accepts only `smp` and rejects `emu`/`jit` (`erlexec: Invalid emulator type or flavor…`, exit 1); `beam_rt` accepts `emu`/`jit`/`smp` so `Flavor::Emu` is reachable.
* **`+K` is validated; the oracle does not.** The emulator only `get_arg`s the value; `beam_rt` accepts the two literals `true`/`false` and rejects anything else.
* **`-M*` vs `+M*`.** Through the `erl` wrapper a `-M*` flag is **not** forwarded to the emulator (`erlexec.c:865`, it falls to `add_arg`), so the oracle never validates it; only `+M*` is rewritten to `-M*` and validated. `beam_rt` recognizes the whole family but validates only `+M*` (oracle parity for `-M*`).
* **`-smp disable` is accepted.** The oracle rejects it outright (`Argument '-smp disable' not supported. Use "+S 1" instead.`); `beam_rt` accepts it as the honest "SMP off" request (the A.6 adoption mandate).

## Constraints
* **Errors are return-based** (workspace-wide): the parser returns `beam_common::Result`; a usage error is a `BeamError` message naming the offending switch/argument. No crate-local error enum.
* **`unsafe`-free** — `../cli.rs` and this directory are plain safe Rust over `std`.
* **`parse` is pure.** It must never read the process environment; only `parse_with_env` does, through an `Env`.

## Test Strategy
* `env.rs` in-module tests: the tokenizer (whitespace runs, quote grouping/concatenation, escapes, `#`, empty input, multi-token expansion) and the merge (order, unset omission, per-variable tokenization, `ERL_FLAGS` before `ERL_ZFLAGS`).
* `flags.rs` in-module tests: the `+Q`/`+P` ranges, the `+sbt` literals, `+K`, `-emu_flavor`, and the `-M*`/`+M*` allocator grammar.
* `../cli.rs` in-module tests: every switch, the init-argument recording rule, the `run` grammar, the trailing-dot `-eval`, and the four exit codes.
* `../../tests/cli_flags.rs` (integration): the **built binary** honours the `ERL_*FLAGS` merge — each variable reaches `init:get_argument/1`, the full merge order, tokenization at the CLI boundary — with golden values pinned from the oracle, plus a direct cross-check against the system `erl` that **skips loudly** when `erl` is unavailable.

## Routing Table
* `env.rs` → the `Env` trait, `ProcessEnv`/`MapEnv`, `tokenize`, `merge_argv`, `otp_flags_var` (the `ERL_*FLAGS` surface)
* `flags.rs` → the validators for the emulator switches with no runtime-visible effect (`+Q`/`+P`/`+K`/`+sbt`/`-emu_flavor`/`-M*`+`+M*`) and their ground-truth constants
* `../cli.rs` → the switch parser (`parse`/`parse_with_env`), the `USAGE` text, the `run` driver and the four `EXIT_*` codes (the parent of this directory)
* `../../tests/cli_flags.rs` → the A.6 integration suite at the executable boundary
