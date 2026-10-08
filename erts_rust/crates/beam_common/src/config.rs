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

//! Runtime configuration: flavor, feature flags, log level and a `RuntimeConfig`
//! skeleton over the emulator start-up surface (compatibility contract A.6).
//!
//! [`RuntimeConfig`] is a **skeleton**: it models the parts of the command line and
//! environment that the Rust ERTS core needs (`-emu_flavor`, `+S`/`+SDcpu`/`+SDio`,
//! `-name`/`-sname`, `-setcookie`, the distribution port range, `+P`, `+MMscs`…),
//! while **unmodeled flags are preserved verbatim** in
//! [`extra_emulator_flags`](RuntimeConfig::extra_emulator_flags) so that nothing is
//! lost. The full `+`/`-`/`-M*`/`ERL_*FLAGS` parser lands in `beam_rt`.

use std::fmt;

/// The execution-engine flavor of the emulator (`-emu_flavor emu|jit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Flavor {
    /// `emu`: the plain interpreter, no JIT (the bootstrap fallback).
    Emu,
    /// `jit`: the default BEAM JIT flavor.
    #[default]
    Jit,
}

impl Flavor {
    /// The command-line spelling of the flavor: `"emu"` or `"jit"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Flavor::Emu => "emu",
            Flavor::Jit => "jit",
        }
    }

    /// Parses a flavor name case-insensitively; `None` for anything else.
    pub fn parse(s: &str) -> Option<Flavor> {
        if s.eq_ignore_ascii_case("emu") {
            Some(Flavor::Emu)
        } else if s.eq_ignore_ascii_case("jit") {
            Some(Flavor::Jit)
        } else {
            None
        }
    }
}

impl fmt::Display for Flavor {
    /// Renders [`Flavor::as_str`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The compile-time/runtime features enabled in this build.
///
/// The defaults describe a normal, non-debugging `jit` emulator; use
/// [`for_flavor`](FeatureFlags::for_flavor) to derive the flags of a flavor and the
/// `with_*` builder setters to adjust individual flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FeatureFlags {
    /// The interpreter back end is available (always true in practice).
    pub interpreter: bool,
    /// The JIT back end is available.
    pub jit: bool,
    /// The optional Cranelift hot tier is available.
    pub cranelift: bool,
    /// Dirty CPU/IO schedulers are enabled (`+SDcpu`/`+SDio`).
    pub dirty_schedulers: bool,
    /// Multiple schedulers (SMP) are enabled.
    pub smp: bool,
    /// The locking assertions of the legacy emulator (`ERL_FLAGS=+l`) are enabled.
    pub lock_checking: bool,
    /// Debug (`debug:assert`) assertions are enabled.
    pub debug_assertions: bool,
    /// Memory instrumentation (`+Mea`-style accounting) is enabled.
    pub memory_instrumentation: bool,
}

impl Default for FeatureFlags {
    /// The defaults of a `jit` emulator: interpreter, JIT, dirty schedulers and SMP
    /// enabled; Cranelift, lock checking, debug assertions and memory
    /// instrumentation disabled.
    fn default() -> Self {
        Self {
            interpreter: true,
            jit: true,
            cranelift: false,
            dirty_schedulers: true,
            smp: true,
            lock_checking: false,
            debug_assertions: false,
            memory_instrumentation: false,
        }
    }
}

impl FeatureFlags {
    /// The default flags for `flavor`.
    ///
    /// [`Flavor::Emu`] disables [`jit`](FeatureFlags::jit) (and therefore the Cranelift
    /// tier, which needs the JIT); every other flag keeps its [`Default`] value.
    pub fn for_flavor(flavor: Flavor) -> Self {
        let mut flags = FeatureFlags::default();
        if flavor == Flavor::Emu {
            flags.jit = false;
            flags.cranelift = false;
        }
        flags
    }

    /// Sets [`interpreter`](FeatureFlags::interpreter).
    pub fn with_interpreter(mut self, enabled: bool) -> Self {
        self.interpreter = enabled;
        self
    }

    /// Sets [`jit`](FeatureFlags::jit).
    pub fn with_jit(mut self, enabled: bool) -> Self {
        self.jit = enabled;
        if !enabled {
            self.cranelift = false;
        }
        self
    }

    /// Sets [`cranelift`](FeatureFlags::cranelift).
    ///
    /// The Cranelift tier is only meaningful with the JIT enabled, so enabling it
    /// here also enables [`jit`](FeatureFlags::jit).
    pub fn with_cranelift(mut self, enabled: bool) -> Self {
        self.cranelift = enabled;
        if enabled {
            self.jit = true;
        }
        self
    }

    /// Sets [`dirty_schedulers`](FeatureFlags::dirty_schedulers).
    pub fn with_dirty_schedulers(mut self, enabled: bool) -> Self {
        self.dirty_schedulers = enabled;
        self
    }

    /// Sets [`smp`](FeatureFlags::smp).
    pub fn with_smp(mut self, enabled: bool) -> Self {
        self.smp = enabled;
        self
    }

    /// Sets [`lock_checking`](FeatureFlags::lock_checking).
    pub fn with_lock_checking(mut self, enabled: bool) -> Self {
        self.lock_checking = enabled;
        self
    }

    /// Sets [`debug_assertions`](FeatureFlags::debug_assertions).
    pub fn with_debug_assertions(mut self, enabled: bool) -> Self {
        self.debug_assertions = enabled;
        self
    }

    /// Sets [`memory_instrumentation`](FeatureFlags::memory_instrumentation).
    pub fn with_memory_instrumentation(mut self, enabled: bool) -> Self {
        self.memory_instrumentation = enabled;
        self
    }
}

/// The runtime log level, ordered from most verbose (`Debug`) to least (`Silent`).
///
/// The severity ordering is `Debug < Info < Warning < Error < Silent`; the configured
/// level is the most verbose level that is emitted, so [`enabled`](crate::enabled) is
/// true for a message level exactly when that message is at least as severe as the
/// configured level. [`LogLevel::Silent`] emits nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
#[non_exhaustive]
pub enum LogLevel {
    /// Verbose tracing (used by `beam_trace!` and `beam_debug!`).
    Debug,
    /// Informational messages (the default).
    #[default]
    Info,
    /// Warnings.
    Warning,
    /// Errors.
    Error,
    /// Logging is disabled entirely.
    Silent,
}
impl LogLevel {
    /// The lowercase name of the level: `"debug"`, `"info"`, `"warning"`, `"error"`
    /// or `"silent"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
            LogLevel::Silent => "silent",
        }
    }

    /// Parses a level name case-insensitively, accepting both `"warn"` and
    /// `"warning"`; `None` for anything else.
    pub fn parse(s: &str) -> Option<LogLevel> {
        if s.eq_ignore_ascii_case("debug") {
            Some(LogLevel::Debug)
        } else if s.eq_ignore_ascii_case("info") {
            Some(LogLevel::Info)
        } else if s.eq_ignore_ascii_case("warn") || s.eq_ignore_ascii_case("warning") {
            Some(LogLevel::Warning)
        } else if s.eq_ignore_ascii_case("error") {
            Some(LogLevel::Error)
        } else if s.eq_ignore_ascii_case("silent") || s.eq_ignore_ascii_case("off") {
            Some(LogLevel::Silent)
        } else {
            None
        }
    }
}

impl fmt::Display for LogLevel {
    /// Renders [`LogLevel::as_str`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The configuration of a running Rust ERTS instance.
///
/// This is a **skeleton over the A.6 CLI/environment surface**: it carries the values
/// the core needs and preserves everything it does not model in
/// [`extra_emulator_flags`](RuntimeConfig::extra_emulator_flags). Flag parsing
/// (`+`/`-`/`-M*`/`ERL_*FLAGS`) itself belongs to `beam_rt`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuntimeConfig {
    /// The execution-engine flavor (`-emu_flavor`).
    pub flavor: Flavor,
    /// The feature flags of this build.
    pub features: FeatureFlags,
    /// The runtime log level.
    pub log_level: LogLevel,
    /// The distribution node name (`-name`/`-sname`), when distribution is enabled.
    pub node_name: Option<String>,
    /// The distribution cookie (`-setcookie`/'erlang.cookie').
    pub cookie: Option<String>,
    /// The inclusive distribution port range (`+Q`/`inet_dist_listen_min`…`max`).
    pub dist_port_range: Option<(u16, u16)>,
    /// The number of normal schedulers (`+S`).
    pub schedulers: Option<u32>,
    /// The number of dirty CPU schedulers (`+SDcpu`).
    pub dirty_cpu_schedulers: Option<u32>,
    /// The number of dirty IO schedulers (`+SDio`).
    pub dirty_io_schedulers: Option<u32>,
    /// The maximum number of processes (`+P`).
    pub max_processes: Option<usize>,
    /// Emulator flags that are not modelled explicitly, preserved verbatim.
    pub extra_emulator_flags: Vec<String>,
}

impl Default for RuntimeConfig {
    /// The defaults of a `jit` runtime: [`FeatureFlags::for_flavor`] of
    /// [`Flavor::Jit`], [`LogLevel::Info`], every optional value unset and no extra
    /// emulator flags.
    fn default() -> Self {
        Self {
            flavor: Flavor::Jit,
            features: FeatureFlags::for_flavor(Flavor::Jit),
            log_level: LogLevel::Info,
            node_name: None,
            cookie: None,
            dist_port_range: None,
            schedulers: None,
            dirty_cpu_schedulers: None,
            dirty_io_schedulers: None,
            max_processes: None,
            extra_emulator_flags: Vec::new(),
        }
    }
}

impl RuntimeConfig {
    /// Creates the default configuration (same as [`Default::default`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the execution-engine flavor, updating the feature flags accordingly.
    pub fn with_flavor(mut self, flavor: Flavor) -> Self {
        self.flavor = flavor;
        self.features = FeatureFlags::for_flavor(flavor);
        self
    }

    /// Sets the runtime log level.
    pub fn with_log_level(mut self, level: LogLevel) -> Self {
        self.log_level = level;
        self
    }

    /// Returns `true` when the configured flavor enables the JIT.
    pub fn is_jit(&self) -> bool {
        self.flavor == Flavor::Jit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_default_parse_and_display() {
        assert_eq!(Flavor::default(), Flavor::Jit);
        assert_eq!(Flavor::parse("emu"), Some(Flavor::Emu));
        assert_eq!(Flavor::parse("EMU"), Some(Flavor::Emu));
        assert_eq!(Flavor::parse("Jit"), Some(Flavor::Jit));
        assert_eq!(Flavor::parse("beam"), None);
        assert_eq!(Flavor::Emu.as_str(), "emu");
        assert_eq!(Flavor::Jit.as_str(), "jit");
        assert_eq!(Flavor::Emu.to_string(), "emu");
        assert_eq!(Flavor::Jit.to_string(), "jit");
    }

    #[test]
    fn feature_flag_defaults() {
        let flags = FeatureFlags::default();
        assert!(flags.interpreter);
        assert!(flags.jit);
        assert!(!flags.cranelift);
        assert!(flags.dirty_schedulers);
        assert!(flags.smp);
        assert!(!flags.lock_checking);
        assert!(!flags.debug_assertions);
        assert!(!flags.memory_instrumentation);
    }

    #[test]
    fn feature_flags_for_flavor() {
        let emu = FeatureFlags::for_flavor(Flavor::Emu);
        assert!(!emu.jit);
        assert!(!emu.cranelift);
        assert!(emu.interpreter);
        assert!(emu.smp);

        let jit = FeatureFlags::for_flavor(Flavor::Jit);
        assert!(jit.jit);
        assert_eq!(jit, FeatureFlags::default());
    }

    #[test]
    fn feature_flag_builders() {
        let flags = FeatureFlags::default()
            .with_cranelift(true)
            .with_lock_checking(true)
            .with_debug_assertions(true)
            .with_memory_instrumentation(true)
            .with_smp(false)
            .with_dirty_schedulers(false)
            .with_interpreter(false);
        assert!(flags.cranelift);
        assert!(flags.lock_checking);
        assert!(flags.debug_assertions);
        assert!(flags.memory_instrumentation);
        assert!(!flags.smp);
        assert!(!flags.dirty_schedulers);
        assert!(!flags.interpreter);
        assert!(flags.jit, "enabling cranelift requires the jit");

        let no_jit = FeatureFlags::default().with_jit(false);
        assert!(!no_jit.jit);
        assert!(!no_jit.cranelift);

        let cranelift_then_jit = FeatureFlags::default().with_cranelift(true).with_jit(false);
        assert!(!cranelift_then_jit.cranelift);
    }

    #[test]
    fn log_level_parse_and_display() {
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("INFO"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("warn"), Some(LogLevel::Warning));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warning));
        assert_eq!(LogLevel::parse("Warning"), Some(LogLevel::Warning));
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("silent"), Some(LogLevel::Silent));
        assert_eq!(LogLevel::parse("off"), Some(LogLevel::Silent));
        assert_eq!(LogLevel::parse("loud"), None);

        assert_eq!(LogLevel::Debug.as_str(), "debug");
        assert_eq!(LogLevel::Info.as_str(), "info");
        assert_eq!(LogLevel::Warning.as_str(), "warning");
        assert_eq!(LogLevel::Error.as_str(), "error");
        assert_eq!(LogLevel::Silent.as_str(), "silent");
        assert_eq!(LogLevel::Warning.to_string(), "warning");
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn log_level_ordering() {
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warning);
        assert!(LogLevel::Warning < LogLevel::Error);
        assert!(LogLevel::Error < LogLevel::Silent);
        assert!(LogLevel::Debug <= LogLevel::Debug);
    }

    #[test]
    fn runtime_config_defaults() {
        let config = RuntimeConfig::default();
        assert_eq!(config, RuntimeConfig::new());
        assert_eq!(config.flavor, Flavor::Jit);
        assert_eq!(config.features, FeatureFlags::for_flavor(Flavor::Jit));
        assert_eq!(config.log_level, LogLevel::Info);
        assert!(config.node_name.is_none());
        assert!(config.cookie.is_none());
        assert!(config.dist_port_range.is_none());
        assert!(config.schedulers.is_none());
        assert!(config.dirty_cpu_schedulers.is_none());
        assert!(config.dirty_io_schedulers.is_none());
        assert!(config.max_processes.is_none());
        assert!(config.extra_emulator_flags.is_empty());
    }

    #[test]
    fn runtime_config_builders() {
        let config = RuntimeConfig::default().with_flavor(Flavor::Emu);
        assert_eq!(config.flavor, Flavor::Emu);
        assert!(!config.features.jit);
        assert!(!config.is_jit());

        let config = RuntimeConfig::new().with_log_level(LogLevel::Warning);
        assert_eq!(config.log_level, LogLevel::Warning);
        assert!(config.is_jit());

        let mut config = RuntimeConfig::new();
        config.extra_emulator_flags.push("+sbwt".to_string());
        assert_eq!(config.extra_emulator_flags, vec!["+sbwt".to_string()]);
    }
}
