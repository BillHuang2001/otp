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

//! A dependency-free logging facade plus the workspace log macros.
//!
//! The level is a single process-global [`AtomicU8`](std::sync::atomic::AtomicU8)
//! (a `LogLevel` discriminant), so logging is usable before, during and after runtime
//! initialisation and costs one relaxed atomic load per call. There is deliberately no
//! external logging crate (`tracing`, `log`, …): this crate is a dependency sink.
//!
//! The five macros ([`beam_trace!`](crate::beam_trace), [`beam_debug!`](crate::beam_debug),
//! [`beam_info!`](crate::beam_info), [`beam_warn!`](crate::beam_warn),
//! [`beam_error!`](crate::beam_error)) format their arguments and write the result to
//! stderr, but only when the corresponding level is enabled. Because this module is
//! private, the macros reach the facade through the crate-root re-exports
//! (`$crate::enabled`, `$crate::LogLevel`), which is what makes them work from every
//! other crate in the workspace.
//!
//! There is no separate `Trace` level: `beam_trace!` maps onto [`LogLevel::Debug`].

use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::LogLevel;

/// The environment variable read by [`init_from_env`].
const LOG_ENV_VAR: &str = "ERTS_RUST_LOG";

// LogLevel discriminants (kept in the `Debug < Info < Warning < Error < Silent` order).
const LEVEL_DEBUG: u8 = 0;
const LEVEL_INFO: u8 = 1;
const LEVEL_WARNING: u8 = 2;
const LEVEL_ERROR: u8 = 3;
const LEVEL_SILENT: u8 = 4;

/// The process-global level; defaults to [`LogLevel::Info`].
static LEVEL: AtomicU8 = AtomicU8::new(LEVEL_INFO);

/// Sets the global log level (an alias of [`set_level`], for start-up symmetry).
pub fn init_logging(level: LogLevel) {
    set_level(level);
}

/// Initialises the level from the `ERTS_RUST_LOG` environment variable.
///
/// The variable is parsed with [`LogLevel::parse`] (case-insensitive, accepting
/// `warn`/`warning`); when it is unset or unparseable the level defaults to
/// [`LogLevel::Info`]. The resulting level is installed globally and returned.
pub fn init_from_env() -> LogLevel {
    let level = std::env::var(LOG_ENV_VAR)
        .ok()
        .and_then(|value| LogLevel::parse(&value))
        .unwrap_or(LogLevel::Info);
    set_level(level);
    level
}

/// Sets the global log level.
pub fn set_level(level: LogLevel) {
    LEVEL.store(to_u8(level), Ordering::Relaxed);
}

/// The current global log level ([`LogLevel::Info`] before any initialisation).
pub fn level() -> LogLevel {
    from_u8(LEVEL.load(Ordering::Relaxed))
}

/// Returns `true` when messages of `level` are currently enabled.
///
/// The check follows the OTP logger rule (`logger_chapter`, "log level check"): the
/// configured level is the *most verbose* level that is emitted, and a message passes
/// when it is at least as severe as that level. [`LogLevel`] is ordered by severity
/// (`Debug < Info < Warning < Error < Silent`), so:
///
/// | configured level | enabled levels |
/// |---|---|
/// | [`Debug`](LogLevel::Debug) | `Debug`, `Info`, `Warning`, `Error` |
/// | [`Info`](LogLevel::Info) (default) | `Info`, `Warning`, `Error` |
/// | [`Warning`](LogLevel::Warning) | `Warning`, `Error` |
/// | [`Error`](LogLevel::Error) | `Error` |
/// | [`Silent`](LogLevel::Silent) | none |
///
/// Consequently [`LogLevel::Debug`] enables everything and [`LogLevel::Silent`]
/// enables nothing: `enabled` returns `false` at and after `Silent`, and
/// `enabled(LogLevel::Silent)` is always `false` because nothing is ever logged at the
/// `Silent` level itself.
pub fn enabled(level: LogLevel) -> bool {
    // `Silent` is not a message level: no message is ever emitted at it.
    if level == LogLevel::Silent {
        return false;
    }
    // `LogLevel` is ordered by severity, so a message is enabled when it is at least as
    // severe as the configured level (`Debug` therefore enables everything).
    level >= self::level()
}

/// The `u8` discriminant of a level, in `Debug < Info < Warning < Error < Silent` order.
const fn to_u8(level: LogLevel) -> u8 {
    match level {
        LogLevel::Debug => LEVEL_DEBUG,
        LogLevel::Info => LEVEL_INFO,
        LogLevel::Warning => LEVEL_WARNING,
        LogLevel::Error => LEVEL_ERROR,
        LogLevel::Silent => LEVEL_SILENT,
    }
}

/// The level of a stored discriminant (anything unknown is treated as `Silent`).
const fn from_u8(value: u8) -> LogLevel {
    match value {
        LEVEL_DEBUG => LogLevel::Debug,
        LEVEL_INFO => LogLevel::Info,
        LEVEL_WARNING => LogLevel::Warning,
        LEVEL_ERROR => LogLevel::Error,
        _ => LogLevel::Silent,
    }
}

/// Logs at [`LogLevel::Debug`] (the trace level) to stderr, if enabled.
///
/// Accepts any `format!` argument list.
#[macro_export]
macro_rules! beam_trace {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::LogLevel::Debug) {
            eprintln!("{}", format!($($arg)*));
        }
    };
}

/// Logs at [`LogLevel::Debug`] to stderr, if enabled.
///
/// Accepts any `format!` argument list.
#[macro_export]
macro_rules! beam_debug {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::LogLevel::Debug) {
            eprintln!("{}", format!($($arg)*));
        }
    };
}

/// Logs at [`LogLevel::Info`] to stderr, if enabled.
///
/// Accepts any `format!` argument list.
#[macro_export]
macro_rules! beam_info {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::LogLevel::Info) {
            eprintln!("{}", format!($($arg)*));
        }
    };
}

/// Logs at [`LogLevel::Warning`] to stderr, if enabled.
///
/// Accepts any `format!` argument list.
#[macro_export]
macro_rules! beam_warn {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::LogLevel::Warning) {
            eprintln!("{}", format!($($arg)*));
        }
    };
}

/// Logs at [`LogLevel::Error`] to stderr, if enabled.
///
/// Accepts any `format!` argument list.
#[macro_export]
macro_rules! beam_error {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::LogLevel::Error) {
            eprintln!("{}", format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// The global level is shared state, so the tests in this module serialise on a
    /// lock (the rest of the crate never touches it).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn level_defaults_to_info() {
        let _guard = lock();
        init_logging(LogLevel::Info);
        assert_eq!(level(), LogLevel::Info);
    }

    #[test]
    fn set_level_round_trips() {
        let _guard = lock();
        for configured in [
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warning,
            LogLevel::Error,
            LogLevel::Silent,
        ] {
            set_level(configured);
            assert_eq!(self::level(), configured);
        }
    }

    #[test]
    fn enabled_follows_the_configured_level() {
        let _guard = lock();

        // `Debug` is the most verbose setting: everything except `Silent` is enabled.
        set_level(LogLevel::Debug);
        assert!(enabled(LogLevel::Debug));
        assert!(enabled(LogLevel::Info));
        assert!(enabled(LogLevel::Warning));
        assert!(enabled(LogLevel::Error));
        assert!(!enabled(LogLevel::Silent));

        // The default: informational and upwards, no debug tracing.
        set_level(LogLevel::Info);
        assert!(!enabled(LogLevel::Debug));
        assert!(enabled(LogLevel::Info));
        assert!(enabled(LogLevel::Warning));
        assert!(enabled(LogLevel::Error));
        assert!(!enabled(LogLevel::Silent));

        set_level(LogLevel::Warning);
        assert!(!enabled(LogLevel::Info));
        assert!(enabled(LogLevel::Warning));
        assert!(enabled(LogLevel::Error));
        assert!(!enabled(LogLevel::Silent));

        set_level(LogLevel::Error);
        assert!(!enabled(LogLevel::Warning));
        assert!(enabled(LogLevel::Error));
        assert!(!enabled(LogLevel::Silent));

        // `Silent` disables everything, including `Silent` itself.
        set_level(LogLevel::Silent);
        assert_eq!(level(), LogLevel::Silent);
        assert!(!enabled(LogLevel::Debug));
        assert!(!enabled(LogLevel::Info));
        assert!(!enabled(LogLevel::Warning));
        assert!(!enabled(LogLevel::Error));
        assert!(!enabled(LogLevel::Silent));

        set_level(LogLevel::Info);
    }

    #[test]
    fn init_from_env_defaults_to_info_when_unset() {
        let _guard = lock();
        std::env::remove_var(LOG_ENV_VAR);
        assert_eq!(init_from_env(), LogLevel::Info);
        assert_eq!(level(), LogLevel::Info);
    }

    #[test]
    fn init_from_env_parses_the_variable() {
        let _guard = lock();

        std::env::set_var(LOG_ENV_VAR, "warn");
        assert_eq!(init_from_env(), LogLevel::Warning);
        assert_eq!(level(), LogLevel::Warning);

        std::env::set_var(LOG_ENV_VAR, "DEBUG");
        assert_eq!(init_from_env(), LogLevel::Debug);
        assert_eq!(level(), LogLevel::Debug);

        std::env::set_var(LOG_ENV_VAR, "not-a-level");
        assert_eq!(init_from_env(), LogLevel::Info);
        assert_eq!(level(), LogLevel::Info);

        std::env::remove_var(LOG_ENV_VAR);
    }

    #[test]
    fn macros_route_through_the_level() {
        let _guard = lock();
        set_level(LogLevel::Silent);

        // Nothing is printed at `Silent`, but the macros must still expand and compile.
        crate::beam_trace!("trace {}", 1);
        crate::beam_debug!("debug {}", 1);
        crate::beam_info!("info {}", 1);
        crate::beam_warn!("warn {}", 1);
        crate::beam_error!("error {}", 1);

        assert!(!enabled(LogLevel::Error));
        assert_eq!(level(), LogLevel::Silent);

        set_level(LogLevel::Info);
    }
}
