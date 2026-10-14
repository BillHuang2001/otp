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
| `-pa Dir` / `-pz Dir` | add `Dir` to the code path: `-pa` **heads** it (each occurrence is prepended in turn, so the head is the `-pa` group in reverse command-line order), `-pz` **tails** it (behind the system `ebin` directories). Applied on **every** route — the bare `-boot` one included — before anything runs |
| `+S N[:N]` | normal scheduler count → `emulator.schedulers` (the first number) |
| `-smp enable\|disable\|auto` | scheduler model; only `disable` changes behaviour |
| `-name N` / `-sname N` | node name → `emulator.node_name`; makes the runtime a distributed node (A.5 phase 1) |
| `-setcookie C` | distribution cookie → `emulator.cookie` (also the init argument `setcookie`) |
| `-config F` | config file, recorded as the init argument `config` |
| `-boot F` | boot script: recorded as the init argument `boot` **and** executed by the bare `Boot` action through the A.7 boot-script executor (a start instruction keeps the preloaded-only floor) |
| `-emu_flavor emu\|jit\|smp` | emulator flavor → `emulator.flavor` + `FeatureFlags` |
| `+A N` | async-thread pool size, preserved verbatim |
| `+P N` | max process count (`legacy`, or `1024..=134217727`) → `emulator.max_processes` |
| `+Q N` | max port count (`legacy`, or `1024..=134217727`), preserved verbatim |
| `+K true\|false` | kernel-poll switch, preserved verbatim |
| `+sbt TYPE` | scheduler bind type (`u`/`db`/`s`/`ps`/`ts`/`tnnps`/`nnps`/`nnts`/`ns`), preserved verbatim |
| `+M*` / `+MM*` / `+MB*` | ERTS allocator flags: `+M*` is an emulator flag — validated against the `erl_alloc.c` grammar and then **discarded** (`../cli.rs`'s allocator arm calls only `flags::parse_allocator`; it does **not** call `Builder::preserve`, so nothing reaches `extra_emulator_flags`, and no init argument is recorded); a user `-M*` is an ordinary unknown `-flag` (recorded as `init:get_argument(NAME)`, its value consumed) |
| `+hms N` / `+hmbs N` / `+hmax N` / `+hmaxk\|el\|ib B` | GC heap policy (`beam_gc::HeapPolicy`), rounded through the growth table; visible in `garbage_collection` |
| `+JPperf VALUE` | perf support (`true`/`false`/`dump`/`map`/`fp`/`no_fp`) → `PerfMapConfig` (validated, preserved verbatim; a bad value is a usage error) |
| `+JPperfdirectory Dir` | where the perf files are written (default `/tmp`; `ERL_SYM_MAP_FILE` overrides the map file's whole path) → `PerfMapConfig` (preserved verbatim) |
| `run <file.beam> <mod>:<func>/<arity> [arg…]` | the vertical slice: load the file, run, print the result |
An unmodeled `-FLAG`/`+FLAG` is **data, not a usage error** (it is preserved, and a `-`-flag is recorded); only a **stray positional argument** (outside `run`) is a usage error (exit 2).
Two [`HeapOptions`](../config.rs) knobs have **no CLI arm** (the emulator has no `+h`-style switch for either, so they are host/test knobs, not part of this surface): the policy's `initial_mature_words` and its `fullsweep_after`. `ERL_FULLSWEEP_AFTER` is parsed into `ErlEnv` but **not** applied; `BEAM_RT_GC_STRATEGY` (the default-OFF `immix` opt-in) is read by `Runtime::new`, never by this parser — neither is an init argument.

### The `-boot` boot action
The bare `Boot` action — the invocation with no `-eval`/`-s`/`-run`/`run` — is the one that drives a boot script: `../cli.rs`'s `run_boot_script` boots the 22 preloaded modules, resolves the `-boot` value exactly as `init`'s `get_boot/2` does and executes the script through the A.7 boot-script executor (`Runtime::boot_from_script`).
Its helpers are `boot_file_argument` (the recorded `boot` init argument, first occurrence wins), `boot_file_root` (the Erlang root above `<lib>`) and `resolve_boot_file` (the `<Name>.boot` candidate plus the string-concatenated `<Root>/bin/<candidate>` fallback; a `$ROOT/…` value is read against the same root).
A missing script is the typed `cannot get bootfile <Name>` error; a malformed script, and a script whose executor meets a directive it cannot run, are typed errors too — all reported `beam_rt: <error>` with exit 1, never a silent skip, a panic or a hang.
The command line's `-pa`/`-pz` switches are already in force when the script starts: `Runtime::new` seeds them into the code server *before* `Runtime::boot` appends the system `ebin` directories, so a `-pa` directory heads the code path and can shadow an installed module for the script's very first `{primLoad, …}` batch. That is the same effective order `init`'s `make_path/4` builds (`Pa ++ ScriptPath ++ Pz`, `erts/preloaded/src/init.erl:1418`), so the two runtimes resolve a boot-script module from the added directory alike; `../../tests/boot_code_path.rs` pins it oracle-cross-checked (and pins that the same directory is honoured on the ordinary `-eval` route). The real `bootstrap/bin/start.boot` runs through directive **18 of 22** — `{apply,{application,start_boot,[kernel,permanent]}}`, at which the kernel application starts and stays up — and **stops at directive 19 of 22**, `{apply,{application,start_boot,[stdlib,permanent]}}`: the kernel application has already been torn down, so directive 19's `{load_application,stdlib}` call finds no `application_controller`. The final `{progress,started}` (index 21) is **not** reached, and `-boot bootstrap/bin/start` exits **1** with `beam_rt: error {noproc,{gen_server,call,[application_controller,{load_application,stdlib},infinity]}}` on stderr. The native `file_server_2` service (`../native/file_server.rs`, which would answer directive 20's `{apply,{c,erlangrc,[]}}`) leaves the name **free** on this route — `Runtime::new` consults `RuntimeConfig::boot_script()` and spawns it unregistered, so the booted `kernel_sup` owns `file_server_2` — and nothing reaches it because the route still stops earlier, at directive 19. Per-directive detail, the mechanism and the currently-failing tests are in `../boot/CONTEXT.md`.
A start instruction (`-eval`/`-s`/`-run`/`run`) keeps the preloaded-only floor, so `-boot` is recorded there but **not** executed.

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
| `ERL_MAX_PORTS` | `atoi`, then **clamped** into `[1024, 134217727]` (`erl_init.c:1341`); sizes the port table | `ErlEnv::max_ports` (the clamped `usize`) **and**, when the merged argv has no `+Q`, `RuntimeConfig::max_ports` (the same request field `+Q N` sets) **and** a preserved `+Q <clamped>` in `extra_emulator_flags` | yes — `system_info(port_limit)` (the power-of-two table sizing on top of the request is `../info`'s) |
| `ERL_INETRC` | path the `inet` resolver reads at start-up | `ErlEnv::inetrc` (verbatim; `Some("")` when set-but-empty) | no — parsed-but-inert (M1 has no resolver) |
| `ERL_CRASH_DUMP` | crash-dump file name (emulator default `erl_crash.dump`) | `ErlEnv::crash_dump` (verbatim) | no — parsed-but-inert (no crash-dump subsystem) |
| `ERL_CRASH_DUMP_SECONDS` | `atoi` (`erl_crash_dump.c:451-457`): `0` = no dump + break now, `< 0` = dump until complete, `> 0` = alarm-bounded | `ErlEnv::crash_dump_seconds` (`i64`; a malformed value is `atoi`'s `0`) | no — parsed-but-inert |
| `ERL_ROOTDIR` | OTP-root relocation honoured by the `erl` **launcher script** | `ErlEnv::rootdir` (verbatim) | no — parsed-but-inert (this binary is not relocatable) |
| `ERL_MAX_ETS_TABLES` | `atoi` (`erl_init.c:1328-1331`); `0` = the emulator default | `ErlEnv::max_ets_tables` (`i64`) | no — parsed-but-inert (no ETS table limit here) |
| `ERL_FULLSWEEP_AFTER` | `atoi` (`erl_init.c:1334-1338`); the emulator stores it in a `Uint16` gen-GC interval | `ErlEnv::fullsweep_after` (`i64`, stored **untruncated** — no gen-GC consumer exists) | no — parsed-but-inert |

**Numeric semantics** are the C's `atoi` (`vars::c_atoi`): leading whitespace, an optional sign, then digits, stopping at the first non-digit — so a non-numeric value is `0`, never a dropped variable and never a wrong value.
`ERL_MAX_PORTS` is the one member with an effect, and it is applied exactly as a `+Q` the merged argv does not already carry:

* the clamp is what the emulator does to the value (`1`/`100`/`bogus` ⇒ 1024, `999999999` ⇒ 134217727); the `2000 ⇒ 2048` step the oracle **observes** in `system_info(port_limit)` is the port table's power-of-two sizing (`erts_ptab_init_table`/`erts_ptab_max`, `erl_ptab.c:383-388`/`erl_ptab.h:266-270`), which `../info`'s `port_limit` applies to every request — this parser models the clamp only;
* a `+Q` **anywhere in the merged argv** — the command line or `ERL_AFLAGS`/`ERL_FLAGS`/`ERL_ZFLAGS` — overrides the variable outright (oracle: `ERL_MAX_PORTS=2048 erl +Q 4096` and `ERL_FLAGS='+Q 4096' erl` both observe 4096), so `ERL_MAX_PORTS` is applied **only** when no `+Q` is present;
* the asymmetry is the oracle's: `+Q` **validates** (and rejects out-of-range), `ERL_MAX_PORTS` **clamps silently**.
`parse` stays pure — it never reads an environment, so it always yields `ErlEnv::default()` and `max_ports: None`; only `parse_with_env` fills `env_vars` and performs the `+Q`/`ERL_MAX_PORTS` routing.
`+Q N` itself sets `RuntimeConfig::max_ports` to the validated request (`+Q legacy` ⇒ `None`, the default table) and preserves the two tokens in `extra_emulator_flags` verbatim.

## Init arguments (`init:get_argument/1`) — the exact recording rule
The parser reproduces the oracle's split between emulator/`init` switches and flags:
* A **`-`-switch** (recognized or not) is recorded as an `InitArgs` occurrence — the source of `init:get_argument/1`.
* A **`+`-flag** is an **emulator** flag and is **not** recorded (the oracle's `init:get_argument('S'|'Q'|'K'|'A'|'P'|'sbt')` is the bare atom `error`).
* The `-eval`/`-s`/`-run` **Start instructions** are consumed by `init` and are **not** recorded (`init:get_argument('eval'|'s'|'run')` is the bare atom `error`).
A flag's name is its switch with the leading `-`/`+` run removed (`--nope` → `nope`, `-pa` → `pa`).
A valueless flag is one occurrence with no values (`-noshell` ⇒ `{ok,[[]]}`); `-pa /a -pa /b` is two occurrences (`{ok,[["/a"],["/b"]]}`).
The oracle additionally reports its own `root`/`bindir`/`progname`/`home` entries in this listing, none of which is an `InitArgs` occurrence here: the native `init:get_argument/1` floor answers `root`/`home`/`progname` from the runtime's own state (`../native/init.rs`) — where `progname` is a deliberate, invocation-dependent gap, each runtime reporting the basename **it** was invoked as — while `bindir` is not modelled.

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
* **`-smp disable` is accepted.** The oracle rejects it outright (`Argument '-smp disable' not supported. Use "+S 1" instead.` — verified OTP 29.1.1 / erts-17.1, exit 1); `beam_rt` accepts it as the honest "SMP off" request (the A.6 adoption mandate). Unlike `+K bogus` and `-emu_flavor emu`, this divergence is pinned only by a unit test (`../cli.rs`), **not** against the oracle in `../../tests/cli_flags.rs` (it is absent from `DIVERGENT_CASES`).
* **`-eval`/`-s`/`-run` are documented subsets.** Supported: the expression subset, the **send operator**, a bare `receive` driving a spawned sender, the spawn probes and a bounded scheduler pump. Gaps (typed errors in `../eval/mod.rs`): a `receive` that never receives, a `receive` inside a spawned process, `after infinity`; real `erl` start-script processing / `-init_debug` are absent.

## Known Issues (verified against the code, not just the docs)
* **`+M*` is validated and dropped, contradicting the source docs.** `../cli.rs`'s allocator arm (`other if flags::is_allocator_switch(other) => index = flags::parse_allocator(...)`) never calls `Builder::preserve`, so unlike every other modelled emulator flag a `+M*` value reaches **neither** `extra_emulator_flags` **nor** any init argument. `../cli.rs`'s own module-table row ("all preserved verbatim") and `USAGE` ("preserved verbatim"), plus `flags.rs`'s module doc ("preserved verbatim in `extra_emulator_flags` so that nothing is lost") and its "-M*/+M* family … records neither as an init argument" claim, are therefore **stale/wrong** for the `+M*` form. Functional impact is nil (allocator flags have no runtime effect here), but the "nothing is lost" promise is not kept.
* **`+JPperf`/`+JPperfdirectory` are implemented but omitted from `USAGE`.** The two switches are parsed, validated, preserved and routed into `PerfMapConfig`, yet `USAGE` does not list them, and `../cli.rs`'s `usage_text_documents_every_supported_switch` is only a spot-check — it also omits `-smp` and the `+hms`/`+hmbs`/`+hmax`/`+hmaxk`/`+hmaxel`/`+hmaxib` heap switches (which *are* in `USAGE`) — so a `USAGE` regression on any unlisted spelling goes unnoticed.
* **Three near-duplicate C-integer parsers, with deliberately different laxness.** `flags.rs::lax_int` (strtol-style, ignores trailing garbage), `vars.rs::c_atoi` (strtol-style, saturating) and `../cli.rs`'s strict `str::parse` family (`parse_u32`/`parse_heap_words`/`parse_max_heap_size`) coexist. `vars.rs::c_atoi` uses `u8::is_ascii_whitespace`, which omits `\x0b` (vertical tab) that C `isspace`/`strtol` skips — so a value like `"\x0b2000"` parses as `0` here, not `2000`; `env.rs::is_whitespace` correctly includes `\x0b`. Minor, but the "exactly the C's `atoi`" doc overstates the match.
* **Action switches are last-wins.** `../cli.rs`'s `parse` assigns `builder.action` per switch, so `-version -eval X` takes the `-eval` action (no banner) and `-eval X -version` takes `Version`. No test or oracle comparison covers a mixed action line.
* **The `ERL_MAX_PORTS` `+Q`-override detection is a raw token scan.** `vars::has_plus_q_switch` matches any `+Q` token in the merged argv, including one sitting in a flag-operand position (e.g. `-pa +Q`), so `ERL_MAX_PORTS` would be suppressed by a token that is not an operative `+Q`. Edge case only.

## Constraints
* **Errors are return-based** (workspace-wide): the parser returns `beam_common::Result`; a usage error is a `BeamError` message naming the offending switch/argument. No crate-local error enum.
* **`unsafe`-free** — `../cli.rs` and this directory are plain safe Rust over `std`.
* **`parse` is pure.** It must never read the process environment; only `parse_with_env` does, through an `Env`.

## Test Strategy
* `env.rs` in-module tests: the tokenizer (whitespace runs, quote grouping/concatenation, escapes, `#`, empty input, multi-token expansion) and the merge (order, unset omission, per-variable tokenization, `ERL_FLAGS` before `ERL_ZFLAGS`).
* `flags.rs` in-module tests: the `+Q`/`+P` ranges, the `+sbt` literals, `+K`, `-emu_flavor`, and the `-M*`/`+M*` allocator grammar.
* `vars.rs` in-module tests: each variable alone and unset (`None`), the `atoi` primitive (`c_atoi`, incl. sign/whitespace/trailing garbage/saturation), the `ERL_MAX_PORTS` clamp (`1`/`100`/`2000`/`999999999`/`bogus`), a malformed `ERL_CRASH_DUMP_SECONDS` and a non-numeric `ERL_MAX_ETS_TABLES`, and the `+Q` token detection.
* `../cli.rs` in-module tests: every switch, the init-argument recording rule, the `run` grammar, the trailing-dot `-eval`, the `-boot` value selection + `init`'s `get_boot/2` resolution (and an absent script ⇒ exit 1), and the four exit codes.
* `../../tests/cli_flags.rs` (integration): the **built binary** honours the `ERL_*FLAGS` merge — each variable reaches `init:get_argument/1`, the full merge order, tokenization at the CLI boundary — with golden values pinned from the oracle, plus a direct cross-check against the system `erl` that **skips loudly** when `erl` is unavailable.
* `../../tests/cli_env.rs` (integration): the second environment family through `beam_rt::cli::parse_with_env` + `MapEnv` (the clamped `+Q` injection **and** the `config.max_ports` request it records, the `+Q`-override cases, `parse`'s purity, every field landing in `config.env_vars()`, and that none becomes an init argument), plus oracle probes of `system_info(port_limit)` that **skip loudly** when `erl` is unavailable. The full `+Q`/`ERL_MAX_PORTS` `port_limit` matrix against this runtime lives in `../../tests/system_info_oracle.rs`.
* `../../tests/cli.rs` (integration): the **built binary**'s exit-code contract and stdout/stderr — the vertical slice (`run` → `5`/`55`, negative/atom/list args), `-eval` silence (trailing `.` tolerated), the unmodeled-switch init argument, the `unsupported` (3) / usage (2) / stray-arg (2) / missing-file (1) codes, `-s`'s atom arguments with a discarded result, the code-path search, and the real structured-raise rendering (`beam_rt: error {badkey,nokey}`).
* `../../tests/boot.rs` (integration): the `-boot FILE` **action** on the built binary — `the_cli_drives_the_boot_script_of_a_boot_flag` (a synthetic script's `apply` step runs, exit 0, empty stderr) and `the_cli_runs_the_real_boot_script_to_completion` (asserts the real `bootstrap/bin/start.boot` exits **0** with empty stdout/stderr — it currently **fails**, because the route stops at directive 19 and exits **1**), plus `a_boot_without_the_boot_flag_keeps_the_preloaded_only_drive`.

## Routing Table
* `env.rs` → the `Env` trait, `ProcessEnv`/`MapEnv`, `tokenize`, `merge_argv`, `otp_flags_var` (the `ERL_*FLAGS` surface)
* `flags.rs` → the validators for the emulator switches with no runtime-visible effect (`+Q`/`+P`/`+K`/`+sbt`/`-emu_flavor`/`-M*`+`+M*`) and their ground-truth constants
* `vars.rs` → the `ErlEnv` model + `from_env` for the `ERL_*` variables **outside** the `*FLAGS` merge (`ERL_MAX_PORTS`/`ERL_INETRC`/`ERL_CRASH_DUMP`/`ERL_CRASH_DUMP_SECONDS`/`ERL_ROOTDIR`/`ERL_MAX_ETS_TABLES`/`ERL_FULLSWEEP_AFTER`)
* `../cli.rs` → the switch parser (`parse`/`parse_with_env`, incl. the `+Q` / `ERL_MAX_PORTS` → `RuntimeConfig::max_ports` request), the `USAGE` text, the `run` driver and the four `EXIT_*` codes (the parent of this directory)
* `../../tests/cli.rs` → the A.6 vertical-slice / exit-code end-to-end suite on the built binary
* `../../tests/cli_flags.rs` → the A.6 integration suite at the executable boundary
* `../../tests/cli_env.rs` → the A.6 integration suite for the non-`*FLAGS` environment variables
* `../../tests/boot.rs` → the A.7/A.9 `-boot FILE` action suite on the built binary (the real `bootstrap/bin/start.boot`, currently stopping at directive **19** — exit **1**)
