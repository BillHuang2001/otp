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

//! The `unsupported` family of [`super::system_info_value`]: the oracle's keys for
//! emulator facilities this runtime genuinely does not have.
//!
//! Every key here is accepted by the oracle but describes a facility that does not exist
//! in this runtime, so answering it with an oracle figure would be a lie about a
//! different emulator. Each returns a raise whose reason atom starts with `unsupported`
//! ([`crate::error::unsupported`]) — the runtime's capability-gap convention — so a
//! caller sees the exit-3 `unsupported:` signal, never a fabricated value.
//!
//! | key | why it is a gap |
//! |---|---|
//! | `allocator` | this runtime does not identify the host C allocator (`{glibc, …}`) |
//! | `time_offset`/`time_correction`/`time_warp_mode`/`tolerant_timeofday` | no time-correction service |
//! | `os_monotonic_time_source` / `os_system_time_source` | no time-source introspection |
//! | `cpu_topology` | no CPU-topology detection |
//! | `check_io` | no poll-set introspection |
//! | `c_compiler_used` | not built by a C compiler |
//! | `embedded_3pps` | embeds none of ERTS's third-party C libraries |
//! | `system_logger` | no logger process to name |
//! | `info` / `procs` / `loaded` | no memory/process/loaded-code self-description dumps |

use beam_common::Result;

use super::{InfoValue, UNSUPPORTED_KEYS};
use crate::error;
use crate::runtime::Runtime;

/// The `system_info/1` value of an `unsupported`-family key, or `None` when `key` is not
/// one this runtime already refuses.
///
/// The answer is always the typed capability gap; the reason names the key so a stderr
/// diagnostic says exactly which facility is missing.
pub(crate) fn try_value(key: &str, _rt: &Runtime) -> Option<Result<InfoValue>> {
    if !UNSUPPORTED_KEYS.contains(&key) {
        return None;
    }
    let reason = match key {
        "allocator" => "this runtime does not identify the host C allocator",
        "time_offset" | "time_correction" | "time_warp_mode" | "tolerant_timeofday" => {
            "this runtime has no time-correction service"
        }
        "os_monotonic_time_source" | "os_system_time_source" => {
            "this runtime has no time-source introspection"
        }
        "cpu_topology" => "this runtime does not detect CPU topology",
        "check_io" => "this runtime has no poll-set introspection",
        "c_compiler_used" => "this runtime was not built by a C compiler",
        "embedded_3pps" => {
            "this runtime embeds none of ERTS's third-party C libraries; \
             its dependencies are Rust crates, a different set this key does not describe"
        }
        "system_logger" => {
            "this runtime runs no logger process, so it has no logger pid to report"
        }
        "info" | "procs" | "loaded" => "this runtime has no self-description dump",
        // Unreachable while `UNSUPPORTED_KEYS` and this match stay in sync; the guard
        // below keeps them honest rather than silently answering `None`.
        _ => "this facility is not implemented",
    };
    Some(Err(error::unsupported(&format!(
        "erlang:system_info({key}): {reason}"
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::test_support::runtime;

    #[test]
    fn every_unsupported_key_reports_a_capability_gap() {
        let rt = runtime();
        for key in UNSUPPORTED_KEYS {
            let value = try_value(key, &rt);
            let error = match value {
                Some(Err(error)) => error,
                other => panic!("{key} must be an unsupported error, got {other:?}"),
            };
            let reason = crate::error::unsupported_reason(&error)
                .unwrap_or_else(|| panic!("{key} must classify as unsupported"));
            assert!(reason.contains(key), "the reason must name {key}: {reason}");
        }
        assert!(try_value("version", &rt).is_none());
        assert!(try_value("nonsense", &rt).is_none());
    }
}
