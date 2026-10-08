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

//! The runtime configuration: the parsed emulator switches plus the selected action.
//!
//! [`RuntimeConfig`] wraps the `beam_common` A.6 **emulator skeleton** ([`EmulatorConfig`])
//! with the things only `beam_rt` owns: the **code path** (`-pa`/`-pz`), the parsed
//! **init arguments** (contract A.6 — the flag surface real `erl` hands to
//! `init:get_argument/1`) and the **action** the CLI was asked to perform
//! (`-version`, `-eval`, `-s`, `-run`, `run`, …). The parser that fills it in lives in
//! [`crate::cli`].

use std::path::PathBuf;

use beam_gc::{next_heap_size, HeapPolicy, DEFAULT_MIN_HEAP_WORDS};

use crate::cli::ErlEnv;
use crate::config::Action::{Boot, Version};
use crate::perf_map::PerfMapConfig;

/// The emulator start-up surface — `beam_common`'s A.6 configuration skeleton.
///
/// `beam_rt` re-exports the type under this name because from the runtime's point of
/// view it *is* the emulator configuration; the full `+`/`-`/`-M*` parser lives here.
pub type EmulatorConfig = beam_common::RuntimeConfig;

/// The default term-heap size in term words (16 Mi words = 128 MiB of address space).
///
/// This is the size of the runtime's term-construction heaps (the BIF-heap pool and the
/// `-eval` heap — see `runtime.rs`). Milestone 1 has **no collector and no heap-growth
/// path** (growth-by-collection is Stage 4), so the heap is a plain bump arena whose
/// capacity *is* the memory limit: a workload that fits, runs; one that does not, raises
/// the emulator's documented `error:system_limit` (never a host-level allocation abort —
/// see `error::enforce_term_heap_limit`).
///
/// A realistic allocation-heavy loop (the benchmark's `lists:seq(1, 1_000_000)`, i.e. 2 Mi
/// words of cons cells) must fit, so the default is set well above the old 64 Ki-word
/// slice budget. The carrier maps the block lazily (`beam_alloc` maps it
/// `MAP_PRIVATE|MAP_ANONYMOUS` and only touched pages become resident), so the 128 MiB of
/// address space costs ~nothing in RSS for a program that allocates little: the bare
/// `beam_rt -noshell -eval 'halt().'` peak RSS is unchanged (~17.8 MB).
pub const DEFAULT_HEAP_WORDS: usize = 1 << 24;

/// The `+hmax*` boolean flags — the C `H_MAX_FLAGS` word (`erl_init.c:814-815`,
/// `erl_init.c:1592-1624`).
///
/// They only change the flag **map** `erlang:process_info(Pid, garbage_collection)`
/// reports for the `max_heap_size` item, not the limit itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxHeapFlags {
    /// `+hmaxk true|false` — kill the process at the limit (the C
    /// `MAX_HEAP_SIZE_KILL`; default `true`).
    pub kill: bool,
    /// `+hmaxel true|false` — log an error report at the limit (the C
    /// `MAX_HEAP_SIZE_LOG`; default `true`).
    pub error_logger: bool,
    /// `+hmaxib true|false` — count off-heap binaries towards the limit (the C
    /// `MAX_HEAP_SIZE_INCLUDE_OH_BINS`; default `false`).
    pub include_shared_binaries: bool,
}

impl Default for MaxHeapFlags {
    /// The emulator defaults (`erl_init.c:814-815`): kill and error-logger on, shared
    /// binaries excluded.
    fn default() -> Self {
        MaxHeapFlags {
            kill: true,
            error_logger: true,
            include_shared_binaries: false,
        }
    }
}

/// The heap-size emulator options — `+hms`, `+hmbs` and the `+hmax*` family — as the
/// [`HeapPolicy`] the runtime's GC introspection reports.
///
/// # The emulator-flag rounding rule
///
/// A `+hms`/`+hmbs` value is a **raw request**, not a final size: the C rounds it through
/// its curated growth table (`erts_next_heap_size(x, 0)`) before it becomes a process's
/// `min_heap_size` / `min_bin_vheap_size`. Verified against the oracle (OTP 29.1.1 /
/// erts-17.1):
///
/// ```text
/// erl +hms 1000 +hmbs 5000 -noshell \
///     -eval 'io:format("~p~n",[process_info(self(),garbage_collection)]), halt().'
///   → min_bin_vheap_size 6772, min_heap_size 1598, fullsweep_after 65535
/// erl -noshell -eval 'io:format("~p~n",[process_info(self(),garbage_collection)]), halt().'
///   → min_bin_vheap_size 46422, min_heap_size 233, fullsweep_after 65535
/// ```
///
/// So [`set_min_heap_words`](HeapOptions::set_min_heap_words) stores
/// `next_heap_size(words, 0)` — `+hms 1000` ⇒ 1598, `+hms 100` ⇒ 142, `+hms 234` ⇒ 376 —
/// with **no** `H_DEFAULT_SIZE` floor (`+hms 100` really does report 142, below the
/// default 233), unlike the `spawn_opt` rule
/// [`HeapPolicy::from_spawn_opts`](beam_gc::HeapPolicy::from_spawn_opts) applies. The
/// `+hms`/`+hmbs` values are *not* recorded in the runtime's init arguments by real `erl`
/// (`erl +hms 1000 … -eval 'init:get_argument(hms)'` ⇒ `error`), but this runtime records
/// every emulator switch, exactly as it already does for `+S`/`+P`/`+A`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapOptions {
    /// The policy the runtime reports through `system_info`/`process_info` — the
    /// emulator defaults with the recognized switches applied.
    policy: HeapPolicy,
    /// The `+hmaxk`/`+hmaxel`/`+hmaxib` flag word.
    max_heap_flags: MaxHeapFlags,
}

impl Default for HeapOptions {
    /// The emulator defaults a process with no `spawn_opt` heap options gets: the C
    /// `H_DEFAULT_SIZE` (233) for `min_heap_size`, `erts_next_heap_size(32768, 0)`
    /// (46422, from `beam_gc`) for `min_bin_vheap_size`, `fullsweep_after` 65535, no max
    /// heap size, and [`MaxHeapFlags::default`].
    ///
    /// The values come from `beam_gc` (the C's own constants and growth table), never from
    /// the oracle's printed figures.
    fn default() -> Self {
        HeapOptions {
            policy: HeapPolicy {
                min_heap_words: DEFAULT_MIN_HEAP_WORDS,
                ..HeapPolicy::default()
            },
            max_heap_flags: MaxHeapFlags::default(),
        }
    }
}

impl HeapOptions {
    /// The emulator defaults (same as [`Default::default`]).
    pub fn new() -> HeapOptions {
        HeapOptions::default()
    }

    /// The policy the runtime's GC introspection reports.
    pub fn policy(&self) -> &HeapPolicy {
        &self.policy
    }

    /// The `+hmax*` flag word.
    pub fn max_heap_flags(&self) -> MaxHeapFlags {
        self.max_heap_flags
    }

    /// Sets the `+hms` minimum heap size, in raw requested words — rounded through the
    /// growth table exactly as the emulator does (see the type docs).
    pub fn set_min_heap_words(&mut self, words: usize) {
        self.policy.min_heap_words = next_heap_size(words, 0);
    }

    /// Sets the `+hmbs` minimum binary virtual heap size, in raw requested words —
    /// rounded through the growth table exactly as the emulator does.
    pub fn set_min_bin_vheap_words(&mut self, words: usize) {
        self.policy.min_bin_vheap_words = next_heap_size(words, 0);
    }

    /// Sets the `+hmax` maximum heap size in words (`0` = unlimited, the emulator's
    /// default). The emulator compares the value against `MAX_SMALL`; the caller — the CLI
    /// — enforces that bound.
    pub fn set_max_heap_words(&mut self, words: usize) {
        self.policy.max_heap_words = words;
    }

    /// Sets the `+hmax*` flag word.
    pub fn set_max_heap_flags(&mut self, flags: MaxHeapFlags) {
        self.max_heap_flags = flags;
    }

    /// [`set_min_heap_words`](HeapOptions::set_min_heap_words), chainable.
    pub fn with_min_heap_words(mut self, words: usize) -> HeapOptions {
        self.set_min_heap_words(words);
        self
    }

    /// [`set_min_bin_vheap_words`](HeapOptions::set_min_bin_vheap_words), chainable.
    pub fn with_min_bin_vheap_words(mut self, words: usize) -> HeapOptions {
        self.set_min_bin_vheap_words(words);
        self
    }

    /// [`set_max_heap_words`](HeapOptions::set_max_heap_words), chainable.
    pub fn with_max_heap_words(mut self, words: usize) -> HeapOptions {
        self.set_max_heap_words(words);
        self
    }

    /// [`set_max_heap_flags`](HeapOptions::set_max_heap_flags), chainable.
    pub fn with_max_heap_flags(mut self, flags: MaxHeapFlags) -> HeapOptions {
        self.set_max_heap_flags(flags);
        self
    }
}

/// The parsed **init arguments**: for every command-line flag, the list of its
/// occurrences, each occurrence being the list of values that followed it.
///
/// This is the shape `init:get_argument/1` reports (contract A.6):
///
/// * `beam_rt -noshell` records `noshell -> [[]]`, so
///   `init:get_argument(noshell)` is `{ok,[[]]}` (a valueless flag);
/// * `beam_rt -pa /a -pa /b` records `pa -> [["/a"], ["/b"]]`, so
///   `init:get_argument(pa)` is `{ok,[["/a"],["/b"]]}`;
/// * a flag that was never given has **no** entry, so `init:get_argument(f)` is the
///   atom `error`.
///
/// Every flag goes in — including the ones this runtime models itself — because a
/// program is entitled to observe its own command line. A flag's name is its switch
/// with the leading `-`/`+` run removed (`--nope` → `nope`, `+Weird` → `Weird`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitArgs {
    occurrences: Vec<(String, Vec<String>)>,
}

impl InitArgs {
    /// No init arguments.
    pub fn new() -> InitArgs {
        InitArgs::default()
    }

    /// Records one occurrence of `flag` with `values`.
    pub fn record(&mut self, flag: &str, values: Vec<String>) {
        self.occurrences.push((flag.to_string(), values));
    }

    /// Every occurrence of `flag`, each as its list of values — `None` when the flag
    /// was never given (which `init:get_argument/1` reports as `error`).
    pub fn get(&self, flag: &str) -> Option<Vec<&[String]>> {
        let found: Vec<&[String]> = self
            .occurrences
            .iter()
            .filter(|(name, _)| name == flag)
            .map(|(_, values)| values.as_slice())
            .collect();
        if found.is_empty() {
            None
        } else {
            Some(found)
        }
    }

    /// The number of recorded flag occurrences.
    pub fn len(&self) -> usize {
        self.occurrences.len()
    }

    /// Whether no flag was recorded.
    pub fn is_empty(&self) -> bool {
        self.occurrences.is_empty()
    }

    /// Every recorded `(flag, values)` occurrence, in command-line order.
    pub fn occurrences(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.occurrences
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }
}

/// One `run FILE MOD:FUNC/ARITY ARGS…` argument, before it is materialized on the
/// runtime heap.
///
/// The grammar (documented on [`crate::cli::parse`]) has four forms: a **small
/// integer** (optionally negative), a **bare atom** (`a`, `abc`, `foo_bar`), a
/// **string** (`"text"`, an Erlang char list) and a **list** of arguments (`[1,a]`).
/// They are parsed and validated by the CLI, and materialized once a [`crate::Runtime`]
/// (whose shared atom table and heap they need) exists — an atom's *index* can only be
/// assigned by the runtime's one atom table, so it cannot be built while parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgTerm {
    /// A small integer literal (may be negative).
    Int(i64),
    /// A bare atom name, interned in the runtime's shared atom table when materialized.
    Atom(String),
    /// A double-quoted string, materialized as an Erlang char list.
    Str(String),
    /// A `[...]` list of arguments, materialized as a proper list.
    List(Vec<ArgTerm>),
}

/// What the runtime was asked to do.
///
/// Exactly one action is selected by the command line; without an action switch the
/// runtime boots and exits ([`Action::Boot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Boot the runtime and exit (`-noshell` with no action switch).
    Boot,
    /// Print the version banner and exit (`-version`/`--version`).
    Version,
    /// Evaluate the documented integer-expression subset and **discard** the result
    /// (`-eval Expr`). Real `erl -eval '2+3'` prints nothing and exits 0.
    Eval(String),
    /// Apply `module:function(AtomArgs…)` and discard the result (`-s Mod Func …`).
    ///
    /// The arguments are the **atom names** given on the command line, exactly as
    /// real `erl` treats them.
    Apply {
        /// The module to call into.
        module: String,
        /// The function to call.
        function: String,
        /// The atom arguments, verbatim from the command line.
        args: Vec<String>,
    },
    /// Apply `module:function(StringArgs…)` and discard the result
    /// (`-run Mod Func …`).
    ///
    /// Unlike `-s`, every argument is a **string** (an Erlang char list) — real
    /// `erl` semantics.
    Run {
        /// The module to call into.
        module: String,
        /// The function to call.
        function: String,
        /// The string arguments, verbatim from the command line.
        args: Vec<String>,
    },
    /// Load a `.beam` file, run `module:function/arity` and print the result
    /// (`run <file.beam> <mod>:<func>/<arity> [args…]`).
    RunFile {
        /// The `.beam` file to load.
        file: PathBuf,
        /// The module the file is expected to define.
        module: String,
        /// The function to run.
        function: String,
        /// The arity to run.
        arity: u32,
        /// The parsed term arguments (materialized on the runtime heap when run).
        args: Vec<ArgTerm>,
    },
}

/// The complete configuration of one `beam_rt` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// The emulator start-up surface of this invocation (`beam_common`'s A.6 skeleton).
    pub emulator: EmulatorConfig,
    /// The code path: `-pa` directories first, then `-pz` directories, in order.
    pub code_paths: Vec<PathBuf>,
    /// The process-heap size in term words.
    pub heap_words: usize,
    /// The heap-size emulator options (`+hms`/`+hmbs`/`+hmax*`) the runtime reports
    /// through its GC introspection (contract A.6 → A.8).
    pub heap: HeapOptions,
    /// The **requested** port-table size: `Some(N)` when the merged command line carries
    /// `+Q N`, or — when it carries no `+Q` — the clamped `ERL_MAX_PORTS` value.
    ///
    /// `None` is the emulator's default table (what a bare invocation and `+Q legacy`
    /// select). The value is exactly what the request was (already inside
    /// `[ERTS_MIN_PORTS, ERTS_MAX_PORTS]`; the `ERL_MAX_PORTS` path clamps in
    /// [`crate::cli::vars`], the `+Q` path rejects out-of-range values outright). The
    /// emulator's **power-of-two table sizing** on top of it is applied where the value
    /// becomes observable: [`crate::info`]'s `port_limit`.
    pub max_ports: Option<usize>,
    /// The parsed init arguments (contract A.6; the source of `init:get_argument/1`).
    pub init_args: InitArgs,
    /// The action to perform.
    pub action: Action,
    /// The **perf-map / JIT-dump** surface `+JPperf` selects (contract A.8).
    ///
    /// The emulator switches accumulate here in command-line order (`+JPperf map` after
    /// `+JPperf false` re-enables the map, exactly as the C's bit accumulator does), and the
    /// **environment override** `ERL_SYM_MAP_FILE` — which the emulator reads directly,
    /// outside the `ERL_*FLAGS` merge, and which is not an `init` argument — is folded in by
    /// [`crate::cli::parse_with_env`]. [`Runtime::new`](crate::Runtime::new) turns the result
    /// into the live files ([`crate::perf_map::PerfMap`]).
    pub perf_map: PerfMapConfig,
    /// The `ERL_*` environment variables read **outside** the `ERL_*FLAGS` merge chain
    /// (contract A.6): `ERL_INETRC`, `ERL_MAX_PORTS`, `ERL_CRASH_DUMP`,
    /// `ERL_CRASH_DUMP_SECONDS`, `ERL_ROOTDIR`, `ERL_MAX_ETS_TABLES` and
    /// `ERL_FULLSWEEP_AFTER`. Empty for the **pure** [`crate::cli::parse`] — only
    /// [`crate::cli::parse_with_env`] reads an environment, and it is where
    /// `ERL_MAX_PORTS` is applied (both as a `+Q` entry of
    /// [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags) and as
    /// the [`max_ports`](Self::max_ports) request).
    pub env: ErlEnv,
}

impl Default for RuntimeConfig {
    /// The defaults of a bare `beam_rt` invocation: a `jit` emulator, an empty code
    /// path, a [`DEFAULT_HEAP_WORDS`]-word heap, the emulator's heap-size defaults, no
    /// init arguments, [`Action::Boot`], no port-table request and no environment
    /// variables.
    fn default() -> Self {
        RuntimeConfig {
            emulator: EmulatorConfig::default(),
            code_paths: Vec::new(),
            heap_words: DEFAULT_HEAP_WORDS,
            heap: HeapOptions::default(),
            max_ports: None,
            init_args: InitArgs::default(),
            action: Boot,
            perf_map: PerfMapConfig::new(),
            env: ErlEnv::default(),
        }
    }
}

impl RuntimeConfig {
    /// The default configuration (same as [`Default::default`]).
    pub fn new() -> Self {
        RuntimeConfig::default()
    }

    /// Sets the action to perform.
    pub fn with_action(mut self, action: Action) -> Self {
        self.action = action;
        self
    }

    /// Replaces the code path with `paths`.
    pub fn with_code_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.code_paths = paths;
        self
    }

    /// Appends `path` to the code path.
    pub fn with_code_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.code_paths.push(path.into());
        self
    }

    /// Sets the process-heap size in term words.
    pub fn with_heap_words(mut self, heap_words: usize) -> Self {
        self.heap_words = heap_words;
        self
    }

    /// Replaces the parsed init arguments.
    pub fn with_init_args(mut self, init_args: InitArgs) -> Self {
        self.init_args = init_args;
        self
    }

    /// Replaces the heap-size emulator options (`+hms`/`+hmbs`/`+hmax*`).
    pub fn with_heap_options(mut self, heap: HeapOptions) -> Self {
        self.heap = heap;
        self
    }

    /// Replaces the emulator start-up surface.
    pub fn with_emulator(mut self, emulator: EmulatorConfig) -> Self {
        self.emulator = emulator;
        self
    }

    /// Sets the number of normal schedulers (`+S`).
    pub fn with_schedulers(mut self, schedulers: u32) -> Self {
        self.emulator.schedulers = Some(schedulers);
        self
    }

    /// Sets the maximum number of processes (`+P`).
    pub fn with_max_processes(mut self, max_processes: usize) -> Self {
        self.emulator.max_processes = Some(max_processes);
        self
    }

    /// Sets the requested port-table size (`+Q N`, or a clamped `ERL_MAX_PORTS`).
    pub fn with_max_ports(mut self, max_ports: usize) -> Self {
        self.max_ports = Some(max_ports);
        self
    }

    /// Sets the distribution node name (`-name`/`-sname`).
    pub fn with_node_name(mut self, node_name: impl Into<String>) -> Self {
        self.emulator.node_name = Some(node_name.into());
        self
    }

    /// Replaces the perf-map / JIT-dump surface (`+JPperf`/`+JPperfdirectory`).
    pub fn with_perf_map(mut self, perf_map: PerfMapConfig) -> Self {
        self.perf_map = perf_map;
        self
    }
    /// The heap-size emulator options (`+hms`/`+hmbs`/`+hmax*`) of this invocation.
    pub fn heap_options(&self) -> &HeapOptions {
        &self.heap
    }

    /// The `ERL_*` environment variables read outside the `ERL_*FLAGS` merge chain
    /// (`ERL_INETRC`, `ERL_MAX_PORTS`, `ERL_CRASH_DUMP`, `ERL_CRASH_DUMP_SECONDS`,
    /// `ERL_ROOTDIR`, `ERL_MAX_ETS_TABLES`, `ERL_FULLSWEEP_AFTER`).
    ///
    /// Empty after the pure [`crate::cli::parse`]; filled in by
    /// [`crate::cli::parse_with_env`].
    pub fn env_vars(&self) -> &ErlEnv {
        &self.env
    }

    /// Replaces the parsed `ERL_*` environment variables.
    pub fn with_env_vars(mut self, env: ErlEnv) -> Self {
        self.env = env;
        self
    }

    /// Whether the configured action is [`Action::Version`].
    pub fn is_version(&self) -> bool {
        self.action == Version
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn defaults() {
        let config = RuntimeConfig::default();
        assert_eq!(config, RuntimeConfig::new());
        assert_eq!(config.emulator, EmulatorConfig::default());
        assert!(config.code_paths.is_empty());
        assert_eq!(config.heap_words, DEFAULT_HEAP_WORDS);
        assert_eq!(config.heap, HeapOptions::default());
        assert_eq!(config.max_ports, None);
        assert!(config.init_args.is_empty());
        assert_eq!(config.action, Action::Boot);
        assert_eq!(config.env_vars(), &ErlEnv::default());
        assert!(!config.is_version());
    }

    /// The emulator defaults, pinned against the oracle (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl -noshell -eval 'io:format("~p~n",[process_info(self(),garbage_collection)]), halt().'
    ///   → [{max_heap_size,#{error_logger=>true,include_shared_binaries=>false,kill=>true,
    ///                       size=>0}},
    ///      {min_bin_vheap_size,46422},{min_heap_size,233},{fullsweep_after,65535},
    ///      {minor_gcs,0}]
    /// ```
    #[test]
    fn default_heap_options_are_the_emulator_defaults() {
        let heap = HeapOptions::default();
        assert_eq!(heap.policy().min_heap_words, 233);
        assert_eq!(heap.policy().min_bin_vheap_words, 46_422);
        assert_eq!(heap.policy().fullsweep_after, 65_535);
        assert_eq!(heap.policy().max_heap_words, 0);
        assert_eq!(
            heap.max_heap_flags(),
            MaxHeapFlags {
                kill: true,
                error_logger: true,
                include_shared_binaries: false,
            }
        );
        // The two figures come from `beam_gc` (the C `H_DEFAULT_SIZE` and the growth
        // table), not from a literal in this crate.
        assert_eq!(heap.policy().min_heap_words, beam_gc::DEFAULT_MIN_HEAP_WORDS);
        assert_eq!(
            heap.policy().min_bin_vheap_words,
            beam_gc::HeapPolicy::default().min_bin_vheap_words
        );
    }

    /// The emulator-flag rounding rule, pinned against the oracle (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl +hms 1000 +hmbs 5000 -noshell \
    ///     -eval 'io:format("~p~n",[process_info(self(),garbage_collection)]), halt().'
    ///   → min_bin_vheap_size 6772, min_heap_size 1598, fullsweep_after 65535
    ///   (and the same probe with +hms 100 / +hms 234 / +hmbs 46423 → 142 / 376 / 75113)
    /// ```
    ///
    /// A requested size is rounded **up** through `beam_gc`'s growth table
    /// (`erts_next_heap_size(x, 0)`) with no `H_DEFAULT_SIZE` floor — `+hms 100` really
    /// does report 142, *below* the 233 default.
    #[test]
    fn heap_options_round_through_the_growth_table() {
        for (requested, rounded) in [
            (100, 142),
            (233, 233),
            (234, 376),
            (1000, 1598),
            (46_423, 75_113),
        ] {
            let heap = HeapOptions::new()
                .with_min_heap_words(requested)
                .with_min_bin_vheap_words(requested);
            assert_eq!(
                heap.policy().min_heap_words,
                rounded,
                "+hms {requested} rounds to {rounded}"
            );
            assert_eq!(
                heap.policy().min_bin_vheap_words,
                rounded,
                "+hmbs {requested} rounds to {rounded}"
            );
            // The rounding is `beam_gc`'s own primitive, not a table in this crate.
            assert_eq!(heap.policy().min_heap_words, beam_gc::next_heap_size(requested, 0));
        }
        // `+hmax` is stored verbatim (`0` = unlimited) and the flag word is a separate knob.
        let bounded = HeapOptions::new().with_max_heap_words(1_000_000);
        assert_eq!(bounded.policy().max_heap_words, 1_000_000);
        assert_eq!(bounded.policy().min_heap_words, 233);
        let flags = MaxHeapFlags {
            kill: false,
            error_logger: false,
            include_shared_binaries: true,
        };
        assert_eq!(
            HeapOptions::new().with_max_heap_flags(flags).max_heap_flags(),
            flags
        );
    }

    #[test]
    fn builders() {
        let config = RuntimeConfig::new()
            .with_action(Action::Version)
            .with_code_path("/code/a")
            .with_code_paths(vec![PathBuf::from("/code/b")])
            .with_heap_words(1024)
            .with_schedulers(2)
            .with_max_processes(1000)
            .with_node_name("beam@host");
        assert_eq!(config.action, Action::Version);
        assert!(config.is_version());
        assert_eq!(config.code_paths, vec![PathBuf::from("/code/b")]);
        assert_eq!(config.heap_words, 1024);
        assert_eq!(config.emulator.schedulers, Some(2));
        assert_eq!(config.emulator.max_processes, Some(1000));
        assert_eq!(config.emulator.node_name.as_deref(), Some("beam@host"));
        assert_eq!(config.heap_options(), &HeapOptions::default());
        // `with_heap_options` replaces the whole heap-size option set.
        let heaps = config
            .clone()
            .with_heap_options(HeapOptions::new().with_max_heap_words(4711));
        assert_eq!(heaps.heap_options().policy().max_heap_words, 4711);
        assert_eq!(heaps.heap_words, 1024);
        // … and `with_emulator` replaces the whole A.6 skeleton, so it is a separate step.
        let replaced = config.with_emulator(EmulatorConfig::default());
        assert_eq!(replaced.emulator, EmulatorConfig::default());
        assert_eq!(replaced.code_paths, vec![PathBuf::from("/code/b")]);
        // The `ERL_*` (non-`*FLAGS`) environment family is a separate, replaceable field.
        let with_env = replaced.with_env_vars(ErlEnv {
            max_ports: Some(1024),
            ..ErlEnv::default()
        });
        assert_eq!(with_env.env_vars().max_ports, Some(1024));
        assert_eq!(with_env.env_vars().inetrc, None);
        assert_eq!(with_env.init_args, InitArgs::default());
    }

    #[test]
    fn init_args_reports_occurrences_and_missing_flags() {
        let mut args = InitArgs::new();
        assert!(args.is_empty());
        // A valueless flag is one occurrence with no values.
        args.record("noshell", Vec::new());
        assert_eq!(args.get("noshell"), Some(vec![&[][..]]));
        // Two occurrences of the same flag stay separate, in order.
        args.record("pa", vec!["/a".to_string()]);
        args.record("pa", vec!["/b".to_string(), "/c".to_string()]);
        assert_eq!(
            args.get("pa"),
            Some(vec![&["/a".to_string()][..], &["/b".to_string(), "/c".to_string()][..]])
        );
        assert_eq!(args.get("absent"), None);
        assert_eq!(args.len(), 3);
        let occurrences: Vec<(&str, &[String])> = args.occurrences().collect();
        assert_eq!(occurrences.len(), 3);
        assert_eq!(occurrences[0].0, "noshell");
        assert_eq!(occurrences[1].0, "pa");
    }

    #[test]
    fn actions_are_distinguishable() {
        assert_ne!(Action::Boot, Action::Version);
        assert_ne!(
            Action::Eval("2+3".to_string()),
            Action::Eval("2*3".to_string())
        );
        assert_ne!(
            Action::Apply {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: Vec::new(),
            },
            Action::Run {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            Action::RunFile {
                file: PathBuf::from("slice.beam"),
                module: "slice".to_string(),
                function: "add".to_string(),
                arity: 2,
                args: vec![ArgTerm::Int(2), ArgTerm::Int(3)],
            },
            Action::RunFile {
                file: PathBuf::from("slice.beam"),
                module: "slice".to_string(),
                function: "add".to_string(),
                arity: 2,
                args: vec![ArgTerm::Int(2), ArgTerm::Int(3)],
            }
        );
    }
}
