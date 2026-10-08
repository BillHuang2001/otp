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

//! The `system` family of [`super::system_info_value`]: build/host identity keys.
//!
//! Every key here is either a genuine **constant** of this build/host (the version
//! strings, the word size, the NIF/driver interface versions, the emulator's fixed
//! feature switches) or is read from the host operating system (`os_type`, `os_version`)
//! — never from the oracle's printed figures. `emu_flavor`, `smp_support` and `creation`
//! are derived from this runtime's own configuration/node.
//!
//! Key shapes (verified against OTP 29.1.1 / erts-17.1):
//!
//! | key | shape |
//! |---|---|
//! | `version` | string, e.g. `"17.1"` |
//! | `otp_release` | string, e.g. `"30"` |
//! | `system_architecture` | string, e.g. `"x86_64-pc-linux-gnu"` |
//! | `system_version` | string |
//! | `nif_version` | string, `"2.18"` |
//! | `driver_version` | string, `"3.3"` |
//! | `wordsize` | int, `8` |
//! | `{wordsize, internal}` | tuple key → int (`size_of::<Eterm>()`) |
//! | `{wordsize, external}` | tuple key → int (`size_of::<usize>()`) |
//! | `emu_flavor` | atom, `jit`/`emu` |
//! | `emu_type` | atom, `opt`/`debug` |
//! | `machine` | string, `"BEAM"` |
//! | `threads` | bool |
//! | `smp_support` | bool |
//! | `async_dist` | bool |
//! | `os_type` | `{unix,linux}` / `{win32,nt}` |
//! | `os_version` | `{Major,Minor,Patch}` |
//! | `break_ignored` | bool |
//! | `debug_compiled` | bool |
//! | `dynamic_trace` | atom, `none` |
//! | `dynamic_trace_probes` | bool |
//! | `modified_timing_level` | atom, `undefined` |
//! | `update_cpu_info` | atom, `unchanged` |
//! | `trace_control_word` | int |
//! | `port_parallelism` | bool |
//! | `kernel_poll` | bool |
//! | `halt_flush_timeout` | atom, `infinity` |
//! | `creation` | int |

use beam_abi::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MINOR_VERSION, ERL_NIF_MAJOR_VERSION,
    ERL_NIF_MINOR_VERSION,
};
use beam_common::{BeamError, Result, ERTS_VERSION};
use beam_term::Eterm;

use super::{os_type, system_architecture, InfoValue};
use crate::error;
use crate::runtime::Runtime;

/// The `system_info/1` value of a `system`-family key, or `None` when `key` is not ours.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "version" => Some(Ok(InfoValue::Str(ERTS_VERSION.to_string()))),
        "otp_release" => Some(Ok(InfoValue::Str(beam_common::otp_version().major.to_string()))),
        "system_architecture" => Some(Ok(InfoValue::Str(system_architecture()))),
        // **Self-derived**: this is *this* runtime's own one-line self-description, with
        // the same shape as the oracle's (version + erts version + build source + word
        // size, each in its own bracket group). The oracle appends further groups derived
        // from emulator state this runtime does not model (the `smp:`/`ds:` scheduler and
        // dirty-scheduler counts, the async-thread pool, the JIT flavor); they are
        // deliberately **omitted** rather than copied or guessed — never a fabricated
        // host/CPU figure. See `src/info/CONTEXT.md`.
        "system_version" => Some(Ok(InfoValue::Str(format!(
            "Erlang/OTP {} [erts-{ERTS_VERSION}] [source] [{}-bit]",
            beam_common::otp_version().major,
            std::mem::size_of::<usize>() * 8,
        )))),
        "nif_version" => Some(Ok(InfoValue::Str(format!(
            "{ERL_NIF_MAJOR_VERSION}.{ERL_NIF_MINOR_VERSION}"
        )))),
        "driver_version" => Some(Ok(InfoValue::Str(format!(
            "{ERL_DRV_EXTENDED_MAJOR_VERSION}.{ERL_DRV_EXTENDED_MINOR_VERSION}"
        )))),
        "wordsize" => Some(Ok(InfoValue::Int(std::mem::size_of::<usize>() as i64))),
        "emu_flavor" => Some(Ok(InfoValue::Atom(rt.config().emulator.flavor.as_str()))),
        "emu_type" => Some(Ok(emu_type())),
        "machine" => Some(Ok(InfoValue::Str("BEAM".to_string()))),
        "threads" => Some(Ok(InfoValue::Bool(true))),
        "smp_support" => Some(Ok(InfoValue::Bool(rt.config().emulator.features.smp))),
        "async_dist" => Some(Ok(InfoValue::Bool(false))),
        "os_type" => Some(Ok(os_type())),
        "os_version" => Some(os_version()),
        "break_ignored" => Some(Ok(InfoValue::Bool(false))),
        "debug_compiled" => Some(Ok(InfoValue::Bool(cfg!(debug_assertions)))),
        "dynamic_trace" => Some(Ok(InfoValue::Atom("none"))),
        "dynamic_trace_probes" => Some(Ok(InfoValue::Bool(false))),
        "modified_timing_level" => Some(Ok(InfoValue::Atom("undefined"))),
        "update_cpu_info" => Some(Ok(InfoValue::Atom("unchanged"))),
        "trace_control_word" => Some(Ok(InfoValue::Int(0))),
        "port_parallelism" => Some(Ok(InfoValue::Bool(false))),
        "kernel_poll" => Some(Ok(InfoValue::Bool(cfg!(unix)))),
        "halt_flush_timeout" => Some(Ok(InfoValue::Atom("infinity"))),
        "creation" => Some(Ok(InfoValue::Int(creation(rt)))),
        _ => None,
    }
}

/// `system_info({wordsize, W})` — the word size of one `W` word, in bytes.
///
/// `erl_bif_info.c:2521-2529`: `internal` is `sizeof(Eterm)` (the term word this runtime's
/// frozen layout pins to one machine word, `../CONTEXT.md` §2) and `external` is
/// `sizeof(UWord)`; any other `W` is the oracle's `badarg`
/// (`erlang:system_info({wordsize,foo})` ⇒ `error:badarg`).
pub(crate) fn wordsize_pair(arg: &str) -> Result<InfoValue> {
    match arg {
        "internal" => Ok(InfoValue::Int(std::mem::size_of::<Eterm>() as i64)),
        "external" => Ok(InfoValue::Int(std::mem::size_of::<usize>() as i64)),
        _ => Err(BeamError::BadArg),
    }
}

/// `system_info(emu_type)` — the build tag the oracle reports: `opt` for an optimized
/// build, `debug` for one compiled with debug assertions.
///
/// Derived from `cfg!(debug_assertions)` (the Rust analogue of the C's
/// `ERTS_DEBUG`/`NDEBUG` build tier), so the answer is honest for *this* build rather
/// than hardcoded from the oracle's `opt`.
fn emu_type() -> InfoValue {
    InfoValue::Atom(if cfg!(debug_assertions) { "debug" } else { "opt" })
}

/// `system_info(creation)` — this node's creation number.
///
/// The value is the running distribution node's creation when `-name`/`-sname` configured
/// one, else `0` (the oracle's `nonode@nohost` creation).
fn creation(rt: &Runtime) -> i64 {
    rt.dist_node().map(|node| i64::from(node.creation())).unwrap_or(0)
}

/// `system_info(os_version)` — the host kernel's `{Major,Minor,Patch}`.
///
/// On Linux the release string is read from `/proc/sys/kernel/osrelease` (the same source
/// the oracle's `uname`-based version comes from) and its leading `Major.Minor.Patch` is
/// parsed. A host where the file is unavailable or unparsable is the honest capability
/// gap rather than a fabricated triple.
fn os_version() -> Result<InfoValue> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|source| {
        error::unsupported(&format!(
            "erlang:system_info(os_version): cannot read the kernel release: {source}"
        ))
    })?;
    let (major, minor, patch) = parse_kernel_release(release.trim()).ok_or_else(|| {
        error::unsupported("erlang:system_info(os_version): unparsable kernel release")
    })?;
    Ok(InfoValue::Tupled(vec![
        InfoValue::Int(major),
        InfoValue::Int(minor),
        InfoValue::Int(patch),
    ]))
}

/// Parses the leading `Major.Minor.Patch` of a kernel release string.
///
/// A component that is absent is `0` (e.g. a `"6.12"` release is `{6,12,0}`); a release
/// whose first two components are not decimal numbers yields `None`.
fn parse_kernel_release(release: &str) -> Option<(i64, i64, i64)> {
    let leading: String = release
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = leading.split('.').map(|part| part.parse::<i64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().transpose().ok().flatten().unwrap_or(0);
    let patch = parts.next().transpose().ok().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::test_support::runtime;
    use beam_common::Flavor;

    #[test]
    fn build_identity_keys_have_the_oracle_shape() {
        let rt = runtime();
        assert_eq!(
            try_value("version", &rt).unwrap().unwrap(),
            InfoValue::Str("17.1".to_string())
        );
        assert_eq!(
            try_value("otp_release", &rt).unwrap().unwrap(),
            InfoValue::Str("30".to_string())
        );
        assert_eq!(
            try_value("nif_version", &rt).unwrap().unwrap(),
            InfoValue::Str("2.18".to_string())
        );
        assert_eq!(
            try_value("driver_version", &rt).unwrap().unwrap(),
            InfoValue::Str("3.3".to_string())
        );
        assert_eq!(
            try_value("wordsize", &rt).unwrap().unwrap(),
            InfoValue::Int(std::mem::size_of::<usize>() as i64)
        );
        assert_eq!(
            try_value("machine", &rt).unwrap().unwrap(),
            InfoValue::Str("BEAM".to_string())
        );
        assert_eq!(
            try_value("os_type", &rt).unwrap().unwrap(),
            InfoValue::Tuple("unix", "linux")
        );
        assert_eq!(
            try_value("system_architecture", &rt).unwrap().unwrap(),
            InfoValue::Str(system_architecture())
        );
        assert!(matches!(
            try_value("emu_type", &rt).unwrap().unwrap(),
            InfoValue::Atom("opt") | InfoValue::Atom("debug")
        ));
        assert!(try_value("nonsense", &rt).is_none());
    }

    #[test]
    fn boolean_and_constant_keys_have_the_oracle_shape() {
        let rt = runtime();
        let boolean = |key: &str| match try_value(key, &rt).unwrap().unwrap() {
            InfoValue::Bool(value) => value,
            other => panic!("{key} must be a bool, got {other:?}"),
        };
        assert!(boolean("threads"));
        assert!(boolean("smp_support"));
        assert!(!boolean("async_dist"));
        assert!(!boolean("break_ignored"));
        assert!(!boolean("dynamic_trace_probes"));
        assert!(!boolean("port_parallelism"));
        assert_eq!(
            try_value("dynamic_trace", &rt).unwrap().unwrap(),
            InfoValue::Atom("none")
        );
        assert_eq!(
            try_value("modified_timing_level", &rt).unwrap().unwrap(),
            InfoValue::Atom("undefined")
        );
        assert_eq!(
            try_value("update_cpu_info", &rt).unwrap().unwrap(),
            InfoValue::Atom("unchanged")
        );
        assert_eq!(
            try_value("halt_flush_timeout", &rt).unwrap().unwrap(),
            InfoValue::Atom("infinity")
        );
        assert_eq!(
            try_value("trace_control_word", &rt).unwrap().unwrap(),
            InfoValue::Int(0)
        );
    }

    #[test]
    fn emu_flavor_follows_the_configured_flavor() {
        // The oracle (OTP 29 / erts-17.1) answers `jit` for its JIT emulator; the Rust
        // runtime ships the Stage-1 baseline JIT and defaults to `Flavor::Jit`.
        let rt = runtime();
        assert_eq!(
            try_value("emu_flavor", &rt).unwrap().unwrap(),
            InfoValue::Atom("jit")
        );

        let mut config = crate::config::RuntimeConfig::default();
        config.emulator = config.emulator.with_flavor(Flavor::Emu);
        let emu = Runtime::new(config).expect("a runtime for the emu flavor");
        assert_eq!(
            try_value("emu_flavor", &emu).unwrap().unwrap(),
            InfoValue::Atom("emu")
        );
    }

    #[test]
    fn creation_is_zero_without_a_node() {
        let rt = runtime();
        assert_eq!(
            try_value("creation", &rt).unwrap().unwrap(),
            InfoValue::Int(0)
        );
    }

    #[test]
    fn parse_kernel_release_reads_the_first_three_components() {
        assert_eq!(parse_kernel_release("7.2.9"), Some((7, 2, 9)));
        assert_eq!(
            parse_kernel_release("6.12.0-091109-generic"),
            Some((6, 12, 0))
        );
        assert_eq!(parse_kernel_release("5.15"), Some((5, 15, 0)));
        assert_eq!(parse_kernel_release("nonsense"), None);
        assert_eq!(parse_kernel_release(""), None);
    }

    #[test]
    fn os_version_is_a_three_integer_tuple_on_linux() {
        // The test host is Linux; the shape must be a 3-int tuple.
        let value = os_version().expect("a Linux host reports os_version");
        match value {
            InfoValue::Tupled(fields) => {
                assert_eq!(fields.len(), 3);
                for field in fields {
                    assert!(matches!(field, InfoValue::Int(value) if value >= 0));
                }
            }
            other => panic!("os_version must be a 3-tuple, got {other:?}"),
        }
    }

    #[test]
    fn kernel_poll_is_true_on_unix() {
        // `beam_io`'s poller uses epoll/kqueue on Unix.
        let rt = runtime();
        assert!(matches!(
            try_value("kernel_poll", &rt).unwrap().unwrap(),
            InfoValue::Bool(true)
        ));
    }
}
