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

//! Shared foundation crate of the Rust ERTS rewrite (Stage 0 — Foundations).
//!
//! `beam_common` is the **lowest common ancestor** of every crate in the `erts_rust`
//! workspace. It owns the things every other crate must agree on:
//!
//! * the workspace **error strategy** — [`BeamError`] / [`Result`], plus the
//!   term-independent BEAM exception machinery ([`Raise`], [`ExitReason`],
//!   [`ExceptionClass`], [`StackTrace`], [`LiveReason`]). Failure is *return-based*: there is no
//!   `setjmp`/`longjmp`, no panic-as-control-flow and no silent swallowing. The engine
//!   turns a [`BeamError`] into the actual raise by storing the exception tag in `x(0)`;
//! * the OTP/ERTS **version constants** ([`OTP_VERSION`], [`ERTS_VERSION`],
//!   [`ERTS_VSN`]) and their parser ([`Version`]);
//! * the **configuration skeleton** over the emulator start-up surface
//!   ([`RuntimeConfig`], [`Flavor`], [`FeatureFlags`], [`LogLevel`]);
//! * a dependency-free **logging facade** ([`init_logging`], [`init_from_env`],
//!   [`set_level`], [`level`], [`enabled`]) with the [`beam_trace!`], [`beam_debug!`],
//!   [`beam_info!`], [`beam_warn!`] and [`beam_error!`] macros;
//! * the documented **`unsafe` marker convention** of the workspace ([`safety`]).
//!
//! This crate is a **dependency sink**: it depends on `std` and `libc` only — no
//! `thiserror`, no `tracing`, no `bitflags` — so that it can sit underneath everything
//! else without pulling a dependency graph along. Its modules are private; every public
//! item is re-exported at the crate root.
//!
//! # Example
//!
//! ```
//! use beam_common::{BeamError, ExitReason, LogLevel, Raise, Version, OTP_VERSION};
//!
//! // Return-based errors, mapped onto BEAM error atoms.
//! assert_eq!(BeamError::BadArg.as_atom(), Some("badarg"));
//! assert_eq!(
//!     BeamError::Raise(Raise::error(ExitReason::atom("badarg"))).as_atom(),
//!     Some("badarg"),
//! );
//!
//! // Versions render in normalized `MAJOR.MINOR.PATCH[-PRERELEASE]` form.
//! assert_eq!(Version::parse(OTP_VERSION).unwrap().to_string(), "30.0.0-rc0");
//! assert_eq!(beam_common::otp_version().major, 30);
//!
//! // Configuration and logging.
//! assert_eq!(LogLevel::parse("warn"), Some(LogLevel::Warning));
//! beam_common::set_level(LogLevel::Info);
//! assert!(beam_common::enabled(LogLevel::Error));
//! ```
//!
//! The log macros are `#[macro_export]`ed, so they resolve from any crate of the
//! workspace through the crate-root `$crate` paths:
//!
//! ```
//! use beam_common::{beam_error, beam_info, beam_warn, enabled, set_level, LogLevel};
//!
//! set_level(LogLevel::Silent);
//! assert!(!enabled(LogLevel::Error), "silent logs nothing");
//! beam_info!("starting {} schedulers", 4);
//! beam_warn!("this line is suppressed");
//! beam_error!("and so is this one");
//! ```
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

mod config;
mod error;
mod logging;
mod version;

/// The workspace `unsafe` marker convention (documentation only).
pub mod safety;

pub use config::*;
pub use error::*;
pub use logging::{enabled, init_from_env, init_logging, level, set_level};
pub use version::*;
