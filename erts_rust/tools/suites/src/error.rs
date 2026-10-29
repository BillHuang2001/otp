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
//! Every failure is *loud* and *typed*: nothing is silently swallowed, and the binary maps
//! `Err` onto a documented process exit code. Hand-rolled (no `thiserror`, no `anyhow`) to
//! keep the crate dependency-free, matching `tools/bench` and `tools/differential`.
use std::fmt;
/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, SuitesError>;
/// The single error type of the `suites` crate.
#[derive(Debug)]
pub enum SuitesError {
    /// A command-line usage error: unknown flag, missing value, bad selector.
    Usage(String),
    /// An environment error: a required prerequisite is missing (no oracle `erl`, no
    /// `rebar3`, a missing source checkout, an unwritable stage directory).
    Env(String),
    /// An I/O error (spawning a process, copying a stage tree, writing a report).
    Io(std::io::Error),
    /// A data error: the harness could not interpret what it observed (a report that does
    /// not match the schema, a suite source tree that cannot be compiled).
    Data(String),
}
impl SuitesError {
    /// The process exit code this error maps onto.
    ///
    /// `2` is the documented "usage / environment" code (the same one `bench` and
    /// `differential` use); `1` means the harness itself failed while running a target.
    pub fn exit_code(&self) -> i32 {
        match self {
            SuitesError::Usage(_) | SuitesError::Env(_) => 2,
            SuitesError::Io(_) | SuitesError::Data(_) => 1,
        }
    }
    /// A one-line, contextualised message for the human.
    pub fn message(&self) -> String {
        match self {
            SuitesError::Usage(m) => format!("usage: {m}"),
            SuitesError::Env(m) => format!("environment: {m}"),
            SuitesError::Io(e) => format!("io: {e}"),
            SuitesError::Data(m) => format!("data: {m}"),
        }
    }
}
impl fmt::Display for SuitesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for SuitesError {}
impl From<std::io::Error> for SuitesError {
    fn from(err: std::io::Error) -> Self {
        SuitesError::Io(err)
    }
}
