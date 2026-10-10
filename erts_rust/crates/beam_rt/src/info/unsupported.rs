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
//! | `build_type` | this runtime's own build tier is not the oracle's release tag |
//! | `allocator` | this runtime does not identify the host C allocator (`{glibc, …}`) |
//! | `time_offset`/`time_correction`/`time_warp_mode`/`tolerant_timeofday` | no time-correction service |
//! | `os_monotonic_time_source` / `os_system_time_source` | no time-source introspection |
//! | `cpu_topology` | no CPU-topology detection |
//! | `check_io` | no poll-set introspection |
//! | `c_compiler_used` | not built by a C compiler |
//! | `embedded_3pps` | embeds none of ERTS's third-party C libraries |
//! | `system_logger` | no logger process to name |
//! | `info` / `procs` / `loaded` | no memory/process/loaded-code self-description dumps |
//! | `compile_info` | not built by a C toolchain, so it has no `ldflags`/`cflags`/`config.h` triple |
//! | `ethread_info` / `ethread_used_tse` | no C ethread library (and no thread-specific-data slots) |
//! | `constant_pool_support` / `beam_jump_table` | no ERTS constant-pool code loading, and no `NO_JUMP_TABLE` build switch to report |
//! | `snifs` / `taints` | no static NIF table, and no NIF-taint tracking |
//! | `emu_args` | the emulator argument vector is not exposed |
//! | `reader_groups_map` / `decentralized_counter_groups_map` | no reader groups and no decentralized counters to distribute |
//! | `max_integer` | no `BIG_ARITY_MAX` bignum-arity cap |
//! | `thread_progress` | no thread-progress debug state to dump |
//!
//! Most keys above are **C-emulator internals**: each describes ERTS's C build, its C
//! thread/allocator library, its loader's code-loading switches or a debug dump of its own
//! C state. None of them has a counterpart this runtime could report a value for, and the
//! C's answer is not a constant of the *interface* (unlike `hipe_architecture`'s
//! `undefined` or `endian`'s host byte order, which are served) — so each is the honest
//! typed capability gap rather than a copied oracle figure.
//!
//! `build_type` is the exception. This runtime is not missing the facility: it *does* have
//! a build tier (`opt` for a release build, `debug` for one with debug assertions — the
//! Rust analogue of the C's `ERTS_DEBUG`/`NDEBUG`). It is a gap because that truthful value
//! is this runtime's **own** tier and does **not** match the oracle's release tag (`opt`):
//! a stable-but-disagreeing own-state value must not be traded for the oracle's `opt`, so
//! the honest typed gap is reported instead.

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
        "build_type" => "this runtime's own build tier is not the oracle's release tag",
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
        "compile_info" => {
            "this runtime is not built by a C toolchain, so it has no ldflags/cflags/config.h triple"
        }
        "ethread_info" => "this runtime has no C ethread library to describe",
        "ethread_used_tse" => "this runtime uses no ethread thread-specific-data slots",
        "constant_pool_support" => {
            "this runtime does not implement ERTS's constant-pool code loading"
        }
        "beam_jump_table" => {
            "this runtime has no ERTS jump-table dispatch build switch (NO_JUMP_TABLE) to report"
        }
        "snifs" => "this runtime publishes no static NIF table",
        "taints" => "this runtime does not track the taints loading a NIF would record",
        "emu_args" => "this runtime does not expose an emulator argument vector",
        "reader_groups_map" => "this runtime has no reader groups to distribute over schedulers",
        "decentralized_counter_groups_map" => {
            "this runtime has no decentralized counters to distribute over schedulers"
        }
        "max_integer" => {
            "this runtime has no BIG_ARITY_MAX bignum-arity cap to report as the largest integer"
        }
        "thread_progress" => "this runtime has no thread-progress debug state to dump",
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
