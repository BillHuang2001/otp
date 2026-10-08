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

//! Tool-level errors of the differential harness.
//!
//! Every variant is a *host-tool* failure — a malformed command line, an environment
//! that cannot satisfy the request, or a failed OS operation — and all of them map onto
//! process exit code `2` (see [`Error::exit_code`]).
//!
//! A BEAM **exception** is *observed data*, never a harness failure: it shows up in the
//! probe's stdout line (`exception <class> <reason>`) and is compared like any other
//! observable result. This is why the harness does not model its own failures with
//! `beam_common::BeamError` (the runtime's error strategy) — the two are different
//! universes: the runtime's errors are the harness's measurements.

use std::fmt;

/// Convenience alias for tool-level results.
pub type Result<T> = std::result::Result<T, Error>;

/// A tool-level failure of the differential harness (always exit code `2`).
#[derive(Debug)]
pub enum Error {
    /// The command line was not understood (unknown subcommand/option, missing argument).
    Usage(String),
    /// The host environment cannot satisfy the request (missing oracle, bad `$BEAM_RT`).
    Env(String),
    /// An operating-system operation (process spawn, file/dir IO) failed.
    Io {
        /// What the harness was trying to do when the operation failed.
        context: String,
        /// The underlying OS error.
        source: std::io::Error,
    },
}

impl Error {
    /// Build a [`Error::Usage`].
    pub fn usage(message: impl Into<String>) -> Error {
        Error::Usage(message.into())
    }

    /// Build an [`Error::Env`].
    pub fn env(message: impl Into<String>) -> Error {
        Error::Env(message.into())
    }

    /// Build an [`Error::Io`].
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// The process exit code this error implies.
    ///
    /// Tool-level errors are always `2` (usage/env/IO), which is distinct from `1`
    /// (one or more differential **mismatches**) and `0` (all executed cases agreed).
    pub fn exit_code(&self) -> i32 {
        2
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(message) => write!(f, "usage error: {message}"),
            Error::Env(message) => write!(f, "environment error: {message}"),
            Error::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Usage(_) | Error::Env(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, Result};

    #[test]
    fn every_tool_error_exits_with_2() {
        let io = Error::io(
            "spawning the oracle",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        );
        assert_eq!(Error::usage("bad flag").exit_code(), 2);
        assert_eq!(Error::env("no erl").exit_code(), 2);
        assert_eq!(io.exit_code(), 2);
    }

    #[test]
    fn display_names_the_failure_class() {
        assert_eq!(
            Error::usage("unknown subcommand").to_string(),
            "usage error: unknown subcommand"
        );
        assert_eq!(Error::env("no erl").to_string(), "environment error: no erl");
        let io = Error::io(
            "spawning the oracle",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        );
        assert_eq!(io.to_string(), "spawning the oracle: no such file");
    }

    #[test]
    fn only_io_errors_carry_a_source() {
        use std::error::Error as _;

        let io = Error::io(
            "reading the report dir",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        assert!(io.source().is_some());
        assert!(Error::usage("x").source().is_none());
        assert!(Error::env("x").source().is_none());
    }

    #[test]
    fn result_alias_is_the_standard_shape() {
        fn ok() -> Result<u8> {
            Ok(1)
        }
        assert_eq!(ok().unwrap(), 1);
    }
}
