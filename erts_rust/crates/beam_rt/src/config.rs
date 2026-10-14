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
//!
//! Two knobs here deliberately sit **outside** that CLI surface, because the emulator has no
//! switch for either: [`HeapOptions::set_initial_mature_words`] (which mature-generation
//! size `beam_gc`'s automatic rule resolves a strategy from) and
//! [`HeapOptions::set_fullsweep_after`]. They are host/test knobs, reachable from a program
//! through `RuntimeConfig::with_heap_options` and from the host through the default-OFF
//! `BEAM_RT_GC_STRATEGY` opt-in ([`GcStrategy`], read by [`strategy_env_default`] and
//! applied once by [`crate::Runtime::new`]) — never as an init argument, never as a flag.

use std::path::PathBuf;
use std::sync::OnceLock;

use beam_gc::{next_heap_size, HeapPolicy, DEFAULT_MIN_HEAP_WORDS, IMMIX_BREAK_EVEN_WORDS};

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

    /// Sets the policy's **initial mature-generation size**, in term words.
    ///
    /// This is `beam_gc::HeapPolicy::initial_mature_words` — the size the collector's mature
    /// space is created with — and it is the **only** input of `beam_gc`'s automatic
    /// mature-strategy rule (`beam_gc::selection::select_mature`, applied by
    /// [`GcConfig::new`](beam_gc::GcConfig::new) inside `AnyCollector::new`): a value
    /// **at or above** [`beam_gc::IMMIX_BREAK_EVEN_WORDS`] resolves to the mark-region
    /// (`MatureKind::Immix`) mature generation, anything **below** it to the copying one
    /// (`MatureKind::Copying`, the default at the emulator's 233 words).
    ///
    /// # The emulator reality
    ///
    /// The emulator has **no `+h`-style switch** for this: `+hms`/`+hmbs`/`+hmax`/`+hmaxk`/
    /// `+hmaxel`/`+hmaxib` are the whole heap-emulator flag surface ([`HeapOptions`]'s type
    /// docs state the rounding rule), and none of them touches the mature-generation size.
    /// So this is a **host/test knob**, not a contract-A.6 CLI surface: it is how a caller
    /// (or the [`GcStrategy`] opt-in) selects a non-default mature strategy without inventing
    /// a flag the oracle does not have. It is additive — [`HeapOptions::default`] is
    /// unchanged by its existence.
    pub fn set_initial_mature_words(&mut self, words: usize) {
        self.policy.initial_mature_words = words;
    }

    /// Sets the policy's **`fullsweep_after`** — the number of minor collections before a
    /// fullsweep is forced (`0` forces one on every collection).
    ///
    /// This is `beam_gc::HeapPolicy::fullsweep_after`, the C `max_gen_gcs`
    /// (`erl_process.h:1145`), whose default is 65535 (`erl_init.c:843`). The emulator's
    /// switch for it *is* the `ERL_FULLSWEEP_AFTER` environment variable, which this runtime
    /// parses into [`ErlEnv`] but does **not** apply — wiring that would
    /// be an oracle-visible behaviour change, out of this knob's scope. Reaching a fullsweep
    /// inside a bounded workload (i.e. driving the mature generation at all) is what a host
    /// uses it for here.
    pub fn set_fullsweep_after(&mut self, after: u32) {
        self.policy.fullsweep_after = after;
    }

    /// Applies a [`GcStrategy`] request to this option set.
    ///
    /// [`GcStrategy::EmulatorDefault`] changes **nothing** (the emulator's own policy, and
    /// the copying mature generation the automatic rule picks below the break-even);
    /// [`GcStrategy::Immix`] raises the policy's initial mature generation to
    /// [`beam_gc::IMMIX_BREAK_EVEN_WORDS`] so the automatic rule resolves to the mark-region
    /// strategy.
    ///
    /// # Why a size, not a pin
    ///
    /// `beam_gc` can be told the strategy explicitly (`GcConfig::immix`), but the host that
    /// spawns processes (`beam_process::ProcessTable::spawn`) accepts only a
    /// [`HeapPolicy`] — there is no `GcConfig`/`MatureSelection` parameter anywhere on the
    /// process or the process table. The policy's mature size is therefore the only lever a
    /// **runtime-configured** strategy has, and driving the automatic rule with it yields the
    /// same observable strategy ([`MatureKind::Immix`](beam_gc::MatureKind::Immix), with
    /// [`MatureSource::Automatic`](beam_gc::MatureSource::Automatic) rather than a pin)
    /// through the one seam that exists.
    pub fn set_gc_strategy(&mut self, strategy: GcStrategy) {
        if let GcStrategy::Immix = strategy {
            self.set_initial_mature_words(IMMIX_BREAK_EVEN_WORDS);
        }
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

    /// [`set_initial_mature_words`](HeapOptions::set_initial_mature_words), chainable.
    pub fn with_initial_mature_words(mut self, words: usize) -> HeapOptions {
        self.set_initial_mature_words(words);
        self
    }

    /// [`set_fullsweep_after`](HeapOptions::set_fullsweep_after), chainable.
    pub fn with_fullsweep_after(mut self, after: u32) -> HeapOptions {
        self.set_fullsweep_after(after);
        self
    }

    /// [`set_gc_strategy`](HeapOptions::set_gc_strategy), chainable.
    pub fn with_gc_strategy(mut self, strategy: GcStrategy) -> HeapOptions {
        self.set_gc_strategy(strategy);
        self
    }
}

/// The **mature-generation strategy** the runtime asks `beam_gc` for when it creates a
/// process.
///
/// `beam_gc` decides a process's mature strategy from the heap policy it is handed: the
/// automatic rule ([`beam_gc::selection::select_mature`]) picks the mark-region
/// (`MatureKind::Immix`) space at/above [`beam_gc::IMMIX_BREAK_EVEN_WORDS`] mature words and
/// the copying space (`MatureKind::Copying`) below it. This enum names the request the
/// *runtime* makes on top of that rule, resolved by [`strategy_from_value`] /
/// [`strategy_env_default`] and applied by [`HeapOptions::set_gc_strategy`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GcStrategy {
    /// The emulator's own behaviour: the configured [`HeapPolicy`] is used exactly as it is
    /// (the default policy's 233 mature words ⇒ the copying mature generation). **The
    /// default, and the value of every unrecognised `BEAM_RT_GC_STRATEGY` setting.**
    #[default]
    EmulatorDefault,
    /// Request the **mark-region** mature generation: raise the policy's initial mature
    /// generation to [`beam_gc::IMMIX_BREAK_EVEN_WORDS`], so `beam_gc`'s automatic rule
    /// resolves to [`MatureKind::Immix`](beam_gc::MatureKind::Immix) — reached through the
    /// rule, never an explicit pin (see [`HeapOptions::set_gc_strategy`]).
    ///
    /// This is a measurement/host opt-in, **not** an emulator default: `beam_gc`'s own
    /// break-even measurement (`beam_gc/MEASURED.md`) is why the mark-region space is not
    /// selected by default.
    Immix,
}

/// [`GcStrategy`] for one `BEAM_RT_GC_STRATEGY` value — the **pure** resolution, and the
/// single definition of that variable's semantics.
///
/// * `Some("immix")` (any ASCII case) → [`GcStrategy::Immix`] — the opt-in.
/// * everything else → [`GcStrategy::EmulatorDefault`], i.e. **no change**: `None` (the
///   variable is unset), the empty string, a named strategy this runtime keeps as its
///   default (`"copying"`), and every unrecognised value. An unknown value is deliberately
///   *not* an error — a typo leaves the emulator behaviour in place rather than silently
///   switching the collector.
///
/// Pure (no environment read), so a test can pin the semantics without mutating the process
/// environment (the crate is `#![deny(unsafe_code)]`, and `set_var` is `unsafe` in edition
/// 2024).
pub fn strategy_from_value(value: Option<&str>) -> GcStrategy {
    match value {
        Some(name) if name.eq_ignore_ascii_case("immix") => GcStrategy::Immix,
        _ => GcStrategy::EmulatorDefault,
    }
}

/// The process-wide GC strategy from the `BEAM_RT_GC_STRATEGY` environment variable.
///
/// The variable's semantics are [`strategy_from_value`]'s; an unset value, the empty string
/// and any unrecognised value are [`GcStrategy::EmulatorDefault`] (today's behaviour), and
/// only `immix` opts in. Read **once** (a `OnceLock`), like `BEAM_RT_TIER`, so a long-running
/// process pays no repeated `getenv`.
///
/// The variable is a **host/test knob**, not part of the A.6 command-line surface: it is
/// applied by [`Runtime::new`](crate::Runtime::new) to the owned configuration **before** it
/// is stored — so it is invisible to `init:get_argument/1` (which reads the parsed
/// [`InitArgs`], untouched) and adds no CLI flag. The pure
/// [`RuntimeConfig::new`]/[`RuntimeConfig::default`] constructors stay environment-free.
pub fn strategy_env_default() -> GcStrategy {
    static DEFAULT: OnceLock<GcStrategy> = OnceLock::new();
    *DEFAULT
        .get_or_init(|| strategy_from_value(std::env::var("BEAM_RT_GC_STRATEGY").ok().as_deref()))
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
    /// The `-pa` directories, in command-line order: the **head** of the code path.
    ///
    /// The oracle's `code_server` *prepends* each `-pa` directory in turn
    /// (`add_pa_pz/3` over `add_paths(first, …)`), so the head of `code:get_path/0` is
    /// this list in **reverse**; a directory that does not exist is filtered out
    /// (`strip_path/2` over `check_path/1`). Both facts are reproduced where the path is
    /// built, not here — this field is what the command line asked for. The `-pz`
    /// directories are the separate [`pz_paths`](Self::pz_paths) tail.
    pub code_paths: Vec<PathBuf>,
    /// The `-pz` directories, in command-line order: the **tail** of the code path.
    ///
    /// The oracle appends them after the system `ebin` directories (and `"."`), so a
    /// `-pz` directory can never shadow an installed module — see
    /// [`code_paths`](Self::code_paths) and
    /// [`CodeServer::set_configured_paths`](crate::CodeServer::set_configured_paths).
    pub pz_paths: Vec<PathBuf>,
    /// The process-heap size in term words.
    pub heap_words: usize,
    /// The **online** scheduler count `+S Total:Online` asks for, or `None` when the
    /// command line gave no explicit online half (`+S N`, or no `+S` at all).
    ///
    /// `+S 4:2` sizes the scheduler pool to `4` but puts only `2` online
    /// (`emulator.schedulers` is the total), which `erlang:system_info(schedulers_online)`
    /// — and the `schedulers_state`/`schedulers_active` rows derived from it — report.
    /// Without an explicit online count the oracle uses the *default* online count,
    /// `min(<total>, <available processors>)`; that resolution lives in
    /// [`crate::info`]'s `schedulers_online`, which this field feeds.
    pub online_schedulers: Option<u32>,
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
    /// The boot script this invocation drives, as the CLI **resolved** it: the explicit
    /// `-boot FILE` value when one was given, else `init`'s own default (the literal
    /// `start`) — for every action but `-version`, which boots nothing.
    ///
    /// This is deliberately a field of its own rather than a read of the `boot` init
    /// argument: `init:get_argument(boot)` must keep reporting what the user actually
    /// typed and never a synthesized default (contract A.6), while a start instruction
    /// like `-eval` still boots the default script. It is `None` for a
    /// [`RuntimeConfig`] a **host** built (`RuntimeConfig::default`,
    /// [`crate::Runtime::with_defaults`]) — such a runtime drives no script unless it
    /// named one with `-boot`.
    pub resolved_boot_script: Option<String>,
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
    /// path (both the `-pa` head and the `-pz` tail), a [`DEFAULT_HEAP_WORDS`]-word heap,
    /// the emulator's heap-size defaults, no init arguments, [`Action::Boot`], no
    /// port-table request, no explicit online scheduler count, no resolved boot script and
    /// no environment variables.
    fn default() -> Self {
        RuntimeConfig {
            emulator: EmulatorConfig::default(),
            code_paths: Vec::new(),
            pz_paths: Vec::new(),
            heap_words: DEFAULT_HEAP_WORDS,
            online_schedulers: None,
            heap: HeapOptions::default(),
            max_ports: None,
            init_args: InitArgs::default(),
            resolved_boot_script: None,
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

    /// Replaces the `-pz` tail of the code path with `paths`.
    pub fn with_pz_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.pz_paths = paths;
        self
    }

    /// Sets the explicit **online** scheduler count (`+S Total:Online`).
    pub fn with_online_schedulers(mut self, online: u32) -> Self {
        self.online_schedulers = Some(online);
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

    /// The `-boot FILE` argument of the merged command line, or `None` when the runtime was
    /// not asked to run a boot script by name.
    ///
    /// The value is read from the recorded init arguments rather than from a field of its own
    /// because that is exactly where `init` reads it: `bootfile/2` resolves `get_flag(boot,
    /// Flags, Root ++ "/bin/start")` (`erts/preloaded/src/init.erl:1257-1258`), and the `boot`
    /// flag *is* the init argument `init:get_argument(boot)` reports (contract A.6). The
    /// **first** occurrence wins (`lists:keyfind/3`, `init.erl:1763`), so `-boot a -boot b`
    /// boots `a`. A `-boot` with no operand never reaches the parsed config: [`crate::cli`]
    /// rejects it as a usage error, so a value is always present when there is an occurrence.
    ///
    /// This is the **name the user typed**, never `init`'s default — see
    /// [`resolved_boot_script`](Self::resolved_boot_script) and
    /// [`boot_script_to_run`](Self::boot_script_to_run) for the script the runtime actually
    /// drives. Overloading this accessor with `init`'s default would be wrong: the `boot` flag
    /// *is* the init argument (contract A.6), and the oracle answers
    /// `init:get_argument(boot)` with `error` (a bare atom) when the user gave no `-boot`, so a
    /// synthesized default here would fabricate an argument the invocation never carried.
    /// The resolved script lives in its own field for exactly that reason.
    pub fn boot_script(&self) -> Option<&str> {
        let occurrences = self.init_args.get("boot")?;
        let values = occurrences.first()?;
        values.first().map(String::as_str)
    }

    /// The boot script the CLI resolved for this invocation, or `None` when it resolved none.
    ///
    /// [`crate::cli`] records the resolved script here **before** it builds the runtime — the
    /// explicit `-boot FILE` value when one was given, else `init`'s literal default `start` —
    /// for every action except `-version`, which never boots (`do_boot/3`,
    /// `erts/preloaded/src/init.erl:1197-1225`). A [`RuntimeConfig`] a host built carries
    /// `None`.
    pub fn resolved_boot_script(&self) -> Option<&str> {
        self.resolved_boot_script.as_deref()
    }

    /// Records [`resolved_boot_script`](Self::resolved_boot_script).
    pub fn with_resolved_boot_script(mut self, script: Option<String>) -> Self {
        self.resolved_boot_script = script;
        self
    }

    /// The script this invocation drives, or `None` when it drives none.
    ///
    /// The CLI's own resolution ([`resolved_boot_script`](Self::resolved_boot_script)) wins;
    /// a config that carries none — a host-built one, a test's — falls back to the `-boot`
    /// init argument, so a caller that hands a parsed `-boot FILE` invocation straight to
    /// [`Runtime::new`](crate::Runtime::new) still measures the script it named.
    pub fn boot_script_to_run(&self) -> Option<&str> {
        self.resolved_boot_script().or_else(|| self.boot_script())
    }

    /// Whether this invocation drives a boot script at all.
    ///
    /// This is the **one** predicate behind both the drive ([`crate::cli`]) and the native
    /// `file_server_2` name decision ([`Runtime::new`](crate::Runtime::new)): a runtime that
    /// boots a script leaves the name to the booted `kernel_sup`'s own `file_server` child,
    /// and one that boots none owns it itself.
    pub fn drives_a_boot_script(&self) -> bool {
        self.boot_script_to_run().is_some()
    }    /// The **last** value the CLI preserved for the emulator switch `switch`, if any.
    ///
    /// Every writer of
    /// [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags)
    /// (`crate::cli`'s `preserve_emulator_flag`) pushes exactly two tokens — the switch
    /// followed by its value — so the list is a flat `(switch, value)` sequence and
    /// stepping by two reads a switch at an even offset with its value at the odd one.
    ///
    /// The **last** occurrence wins: the C's `argv` loop overwrites its variable as it
    /// walks, so a repeated `+A`/`+t`/`+e`/`+sbt` takes the final value (verified against
    /// OTP 29.1.1 / erts-17.1 for `+A 2 +A 4`, which reports the async pool as `4`).
    ///
    /// A truncated pair (the value token missing) contributes nothing — never a panic.
    /// Use [`preserved_emulator_flags`](Self::preserved_emulator_flags) when the switch's
    /// semantics is a *union* over its occurrences rather than last-one-wins.
    pub fn preserved_emulator_flag(&self, switch: &str) -> Option<&str> {
        self.emulator
            .extra_emulator_flags
            .chunks_exact(2)
            .filter(|pair| pair[0] == switch)
            .next_back()
            .map(|pair| pair[1].as_str())
    }

    /// Every value the CLI preserved for the emulator switch `switch`, in command-line
    /// order (see [`preserved_emulator_flag`](Self::preserved_emulator_flag) for the
    /// channel's `(switch, value)` shape).
    pub fn preserved_emulator_flags<'a>(
        &'a self,
        switch: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.emulator
            .extra_emulator_flags
            .chunks_exact(2)
            .filter(move |pair| pair[0] == switch)
            .map(|pair| pair[1].as_str())
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

    /// The two host/test knobs added to [`HeapOptions`] are **additive**: the emulator
    /// defaults ([`HeapOptions::default`]) are byte-identical with them present, and each
    /// setter/`with_*` round-trips exactly one policy field.
    ///
    /// The default mature-generation size is the C's `H_DEFAULT_SIZE` (233) — the same figure
    /// the default `min_heap_size` uses — and the default `fullsweep_after` is 65535
    /// (`erl_init.c:843`), so the automatic mature-strategy rule keeps the copying space
    /// (233 < [`beam_gc::IMMIX_BREAK_EVEN_WORDS`]).
    #[test]
    fn mature_size_and_fullsweep_are_additive_host_knobs() {
        let default = HeapOptions::default();
        assert_eq!(default.policy().initial_mature_words, 233);
        assert_eq!(default.policy().fullsweep_after, 65_535);
        assert_eq!(
            default.policy().initial_mature_words,
            beam_gc::HeapPolicy::default().initial_mature_words
        );
        // `new()` and `default()` agree, and neither knob changes anything else.
        assert_eq!(HeapOptions::new(), default);
        assert_eq!(default.clone().with_initial_mature_words(233), default);
        assert_eq!(default.clone().with_fullsweep_after(65_535), default);

        // The setters round-trip.
        let mut heap = HeapOptions::new();
        heap.set_initial_mature_words(IMMIX_BREAK_EVEN_WORDS);
        heap.set_fullsweep_after(1);
        assert_eq!(heap.policy().initial_mature_words, IMMIX_BREAK_EVEN_WORDS);
        assert_eq!(heap.policy().fullsweep_after, 1);
        // … and only those fields moved.
        assert_eq!(heap.policy().min_heap_words, default.policy().min_heap_words);
        assert_eq!(
            heap.policy().min_bin_vheap_words,
            default.policy().min_bin_vheap_words
        );
        assert_eq!(heap.policy().max_heap_words, default.policy().max_heap_words);
        assert_eq!(heap.max_heap_flags(), default.max_heap_flags());
        // The `with_*` forms chain to the same state.
        assert_eq!(
            HeapOptions::new()
                .with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS)
                .with_fullsweep_after(1),
            heap
        );
    }

    /// `BEAM_RT_GC_STRATEGY`'s semantics, pinned **without** touching the process
    /// environment (a test that set the real variable would race every other test in this
    /// binary, and `set_var` is `unsafe`).
    #[test]
    fn gc_strategy_from_value_is_the_single_definition() {
        // Unset, empty, the default strategy named explicitly and any typo all mean "no
        // change" — never an error and never a silent switch of the collector.
        for value in [None, Some(""), Some("copying"), Some("nope"), Some("im mix")] {
            assert_eq!(
                strategy_from_value(value),
                GcStrategy::EmulatorDefault,
                "{value:?} must leave the emulator behaviour in place"
            );
        }
        // `immix` opts in, in any ASCII case.
        for value in ["immix", "Immix", "IMMIX"] {
            assert_eq!(strategy_from_value(Some(value)), GcStrategy::Immix);
        }
        // The default value of the type is the emulator behaviour.
        assert_eq!(GcStrategy::default(), GcStrategy::EmulatorDefault);
    }

    /// Applying a strategy moves **exactly** the mature-generation size — the one lever a
    /// runtime-configured strategy has (`ProcessTable::spawn` takes only a `HeapPolicy`, so
    /// there is no `MatureSelection` pin to set).
    #[test]
    fn applying_a_gc_strategy_only_moves_the_mature_size() {
        let default = HeapOptions::default();
        // The emulator default changes nothing at all.
        assert_eq!(default.clone().with_gc_strategy(GcStrategy::EmulatorDefault), default);
        // The opt-in raises the mature generation to the rule's break-even (so the automatic
        // rule resolves to the mark-region strategy) and nothing else.
        let immix = default.clone().with_gc_strategy(GcStrategy::Immix);
        assert_eq!(immix.policy().initial_mature_words, IMMIX_BREAK_EVEN_WORDS);
        assert_eq!(immix.policy().min_heap_words, default.policy().min_heap_words);
        assert_eq!(immix.policy().fullsweep_after, default.policy().fullsweep_after);
        assert_eq!(
            immix.policy().min_bin_vheap_words,
            default.policy().min_bin_vheap_words
        );
        // A `set_gc_strategy` on an already-configured option set is equivalent.
        let mut heap = HeapOptions::default();
        heap.set_gc_strategy(GcStrategy::Immix);
        assert_eq!(heap, immix);
        // The break-even really is the automatic rule's switch-over point.
        assert_eq!(
            beam_gc::select_mature(immix.policy().initial_mature_words),
            beam_gc::MatureKind::Immix
        );
        assert_eq!(
            beam_gc::select_mature(default.policy().initial_mature_words),
            beam_gc::MatureKind::Copying
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
