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

//! Crate error and result types.
//!
//! Every failure in `bench` is *loud* and *typed*: nothing is silently swallowed. The
//! binary maps `Err` onto process exit code [`BenchError::exit_code`] (always `2`, the
//! documented "usage/env/IO/schema error" code for this tool) and prints the message to
//! stderr. There is no `anyhow`, no `thiserror` — this is a hand-rolled, dependency-free
//! error enum per the crate's "no new deps" constraint.

use std::fmt;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, BenchError>;

/// The single error type of the `bench` crate.
#[derive(Debug)]
pub enum BenchError {
    /// A command-line usage error: unknown flag, missing argument, or an
    /// unparseable value (e.g. `--runtime bogus`).
    Usage(String),
    /// An environment error: a required external prerequisite is missing (most
    /// notably an unresolvable oracle runtime for `run`/`baseline`).
    Env(String),
    /// An I/O error (spawning a process, reading/writing the results file, …).
    Io(std::io::Error),
    /// A JSON (de)serialization error.
    Serde(serde_json::Error),
    /// A result-schema error: the loaded JSON is structurally valid JSON but does
    /// not describe a `bench` report of the expected shape/version.
    Schema(String),
}

impl BenchError {
    /// Builds a [`BenchError::Usage`].
    pub fn usage(message: impl Into<String>) -> Self {
        BenchError::Usage(message.into())
    }

    /// Builds a [`BenchError::Env`].
    pub fn env(message: impl Into<String>) -> Self {
        BenchError::Env(message.into())
    }

    /// Builds a [`BenchError::Schema`].
    pub fn schema(message: impl Into<String>) -> Self {
        BenchError::Schema(message.into())
    }

    /// The process exit code this error maps onto.
    ///
    /// `2` for *every* variant: the CLI contract reserves `2` for
    /// "usage/env/IO/schema error" and `1` is never produced by `bench` itself
    /// (only by an *oracle-confirmed* differential mismatch, which lives in the
    /// sibling `differential` crate).
    pub fn exit_code(&self) -> i32 {
        2
    }
}

impl fmt::Display for BenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BenchError::Usage(m) => write!(f, "usage error: {m}"),
            BenchError::Env(m) => write!(f, "environment error: {m}"),
            BenchError::Io(e) => write!(f, "io error: {e}"),
            BenchError::Serde(e) => write!(f, "json error: {e}"),
            BenchError::Schema(m) => write!(f, "schema error: {m}"),
        }
    }
}

impl std::error::Error for BenchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BenchError::Io(e) => Some(e),
            BenchError::Serde(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BenchError {
    fn from(value: std::io::Error) -> Self {
        BenchError::Io(value)
    }
}

impl From<serde_json::Error> for BenchError {
    fn from(value: serde_json::Error) -> Self {
        BenchError::Serde(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_is_always_two() {
        assert_eq!(BenchError::usage("x").exit_code(), 2);
        assert_eq!(BenchError::env("x").exit_code(), 2);
        assert_eq!(BenchError::schema("x").exit_code(), 2);
        assert_eq!(
            BenchError::from(std::io::Error::new(std::io::ErrorKind::Other, "boom")).exit_code(),
            2
        );
    }

    #[test]
    fn display_is_prefixed_and_mentions_the_cause() {
        assert_eq!(
            BenchError::usage("bad flag").to_string(),
            "usage error: bad flag"
        );
        assert_eq!(BenchError::env("no erl").to_string(), "environment error: no erl");
        assert!(BenchError::schema("nope").to_string().contains("nope"));
    }

    #[test]
    fn conversion_helpers_round_trip_through_source() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err = BenchError::from(io);
        assert!(std::error::Error::source(&err).is_some());
        assert!(matches!(err, BenchError::Io(_)));

        let serde_err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = BenchError::from(serde_err);
        assert!(matches!(err, BenchError::Serde(_)));
    }
}
