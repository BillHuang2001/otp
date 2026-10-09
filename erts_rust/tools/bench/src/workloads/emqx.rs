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

//! `emqx` — the messaging/copy-bound workload (the EMQX broker build).
//!
//! Stages the EMQX checkout into a private scratch copy under `$TMPDIR` (the source repo is
//! configured **read-only**) and drives EMQX's **real** build there, recording wall time
//! (and peak memory when measurable). Repo: `$BENCH_EMQX_REPO`, else
//! `/home/bill/Source/emqx`.
//!
//! EMQX is a **`make`/`mix` umbrella** (Elixir `apps/`, no root `rebar.config`), so its build
//! entry point is the Makefile's `<profile>-compile` target — which bootstraps the EMQX
//! `rebar3` fork, runs `mix deps.get` (network) and then `mix compile` — driven here through
//! the repo's own `./mix` wrapper. A `rebar3 compile` at the root would verify dependencies,
//! compile *nothing* and still exit `0`: a silent no-op that must never be recorded as a
//! broker-build number. That shape is rejected up front (`not-an-emqx-project`), so this
//! workload can only ever report a number it actually measured or an explicit skip/failure.
//!
//! The build needs a writable `$HOME` (Mix/Hex caches), so the child is given a
//! `HOME`/`MIX_HOME`/`HEX_HOME` under the scratch dir: the read-only checkout and the
//! developer's real home are never touched.
//!
//! **Toolchain gate.** EMQX pins the Erlang/OTP release it builds with (`env.sh`'s `OTP_VSN`,
//! `.tool-versions`) and does not build on a newer toolchain. This checkout pins OTP 28;
//! under the OTP 29 `erl` on `PATH` the `:ezstd` dependency's NIF makefile writes its
//! `env.mk` with `erl -noshell -s init stop -eval …`, an idiom OTP 29 no longer evaluates (it
//! halts first), so `mix compile` aborts with *No rule to make target '…/env.mk'*. Rather
//! than spend minutes discovering that, the mismatch is detected cheaply up front and recorded
//! as an honest `skipped(emqx-requires-otp-<n>)` (with the mechanism in the notes); when the
//! toolchain *does* match the checkout's pin, the real build runs and the row carries a real
//! wall time — or `failed(emqx-build-failed)` with the captured error. No number is ever
//! fabricated.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::{resolve_repo, tool};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/emqx";

/// The build tool EMQX's umbrella is compiled with (the repo's `./mix` wrapper execs this).
const MIX: &str = "mix";

/// The honest skip reason reported when [`MIX`] cannot be resolved.
const MIX_ABSENT: &str = "mix-absent";

/// The honest skip reason reported when `make` (the Makefile-driven build entry point)
/// cannot be resolved.
const MAKE_ABSENT: &str = "make-absent";

/// The honest skip reason reported when the checkout is not an EMQX umbrella (no root
/// `mix.exs`), so the `mix`/Makefile build entry points do not exist.
const NOT_AN_EMQX_PROJECT: &str = "not-an-emqx-project";

/// The honest failure reason reported when the real build ran but did not succeed.
const BUILD_FAILED: &str = "emqx-build-failed";

/// The build profile compiled by this workload — the checkout's own default (`Makefile`
/// `PROFILE ?= emqx-enterprise` and `REL_PROFILES := emqx-enterprise`). The compiled target
/// is therefore `emqx-enterprise-compile`.
const BUILD_PROFILE: &str = "emqx-enterprise";

/// How many trailing characters of a child's captured stderr are kept in a note: a full
/// umbrella build can emit megabytes, which must not bloat the report.
const NOTE_TAIL_CHARS: usize = 2000;

/// The EMQX workload.
pub struct Emqx;

fn repo() -> PathBuf {
    resolve_repo("emqx", DEFAULT_REPO)
}

/// Whether `repo` is an EMQX umbrella — it carries a root `mix.exs`. A directory without one
/// has no `mix`/Makefile build entry point, so the workload must not pretend to build it.
fn is_emqx_project(repo: &Path) -> bool {
    repo.join("mix.exs").is_file()
}

/// Builds the command that drives EMQX's real build: `make <profile>-compile` in the staged
/// copy, with Mix/Hex caches redirected into `home` so nothing is written to the read-only
/// checkout or the developer's `$HOME`.
fn build_command(make: &Path, work: &Path, home: &Path, profile: &str) -> Command {
    let mut cmd = Command::new(make);
    cmd.current_dir(work)
        .arg(format!("{profile}-compile"))
        .env("HOME", home)
        .env("MIX_HOME", home.join(".mix"))
        .env("HEX_HOME", home.join(".hex"));
    cmd
}

/// The trailing `max` characters of `text` (trimmed), for a readable note.
fn tail(text: &str, max: usize) -> &str {
    let text = text.trim();
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The leading integer of a version string (`"28.5.0.7-2"` → `28`), `None` when it does not
/// start with a digit.
fn leading_major(version: &str) -> Option<u32> {
    let digits: String = version
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The OTP major release this EMQX checkout builds with, taken from its own declaration:
/// `env.sh`'s `OTP_VSN` (`export OTP_VSN=28.5.0.7-2`), else `.tool-versions`' `erlang` line.
/// `None` when neither is present/parseable, in which case no toolchain gate is applied.
fn required_otp_major(repo: &Path) -> Option<u32> {
    if let Ok(text) = std::fs::read_to_string(repo.join("env.sh")) {
        for line in text.lines() {
            if let Some((_, value)) = line.split_once("OTP_VSN=") {
                if let Some(major) = leading_major(value) {
                    return Some(major);
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(repo.join(".tool-versions")) {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("erlang ") {
                if let Some(major) = leading_major(rest) {
                    return Some(major);
                }
            }
        }
    }
    None
}

/// The OTP major release of the `erl` on `PATH`, cached for the process. `None` when `erl`
/// cannot be resolved or queried — the gate is then skipped and the real build is attempted.
fn host_otp_major() -> Option<u32> {
    static CACHE: OnceLock<Option<u32>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let erl = proc::which("erl")?;
        let mut cmd = Command::new(erl);
        cmd.args([
            "-noshell",
            "-eval",
            "io:format(\"~s\", [erlang:system_info(otp_release)]), halt().",
        ]);
        let out = proc::run(&mut cmd).ok()?;
        if !out.success {
            return None;
        }
        out.stdout.trim().parse().ok()
    })
}

/// The honest skip reason when the host OTP (`host`) is newer than the release the checkout
/// builds with (`required`), else `None`. Pure, so the decision is unit-testable.
fn otp_gate_reason(required: u32, host: u32) -> Option<String> {
    (host > required).then(|| format!("emqx-requires-otp-{required}"))
}

/// The note explaining an [`otp_gate_reason`] skip, naming the observed build breakage.
fn otp_gate_note(required: u32, host: u32) -> String {
    format!(
        "EMQX builds with Erlang/OTP {required} (env.sh OTP_VSN / .tool-versions), but `erl` on PATH \
         is OTP {host}: the umbrella build aborts in a dependency NIF (:ezstd) whose makefile generates \
         env.mk via `erl -noshell -s init stop -eval …`, an idiom OTP {host} no longer evaluates, so \
         `mix compile` fails with \"No rule to make target '…/env.mk'\"; no number is fabricated"
    )
}

/// The `(reason, note)` of the toolchain gate, or `None` when the host can build this
/// checkout (or the gate cannot be evaluated).
fn toolchain_gate(repo: &Path) -> Option<(String, String)> {
    let required = required_otp_major(repo)?;
    let host = host_otp_major()?;
    let reason = otp_gate_reason(required, host)?;
    Some((reason, otp_gate_note(required, host)))
}

impl Workload for Emqx {
    fn id(&self) -> &'static str {
        "emqx"
    }

    fn description(&self) -> &'static str {
        "EMQX broker build (make emqx-enterprise-compile: mix deps.get + mix compile) — messaging/copy bound"
    }

    fn category(&self) -> Category {
        Category::Messaging
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.9"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        let r = repo();
        if !r.is_dir() {
            return Probe::skipped("repo-missing");
        }
        if !is_emqx_project(&r) {
            return Probe::skipped(NOT_AN_EMQX_PROJECT);
        }
        if !tool::available(MIX) {
            return Probe::skipped(MIX_ABSENT);
        }
        if proc::which("make").is_none() {
            return Probe::skipped(MAKE_ABSENT);
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let r = repo();
        if !r.is_dir() {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "repo-missing",
            );
        }
        if !is_emqx_project(&r) {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                NOT_AN_EMQX_PROJECT,
            );
        }
        let Some(make) = proc::which("make") else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                MAKE_ABSENT,
            );
        };
        if !tool::available(MIX) {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                MIX_ABSENT,
            );
        }

        // Cheap toolchain gate: an OTP newer than the release EMQX pins is a documented
        // non-buildable combination, so record the exact blocker instead of discovering it
        // minutes into the build.
        if let Some((reason, note)) = toolchain_gate(&r) {
            let mut result = WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                reason,
            );
            result.notes.push(note);
            return result;
        }

        // Stage the read-only checkout into a private scratch copy and build there, so the
        // build never writes artifacts (or Mix caches) into the source repository.
        let scratch = match ScratchDir::new(self.id()) {
            Ok(dir) => dir,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("scratch-dir-failed: {e}"),
                )
            }
        };
        let work = scratch.path().join(self.id());
        if let Err(e) = scratch::copy_tree(&r, &work, tool::REPO_SKIP_NAMES) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("scratch-copy-failed: {e}"),
            );
        }
        // A writable HOME for Mix/Hex, inside the scratch dir.
        let home = scratch.path().join("home");
        if let Err(e) = std::fs::create_dir_all(&home) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("scratch-home-failed: {e}"),
            );
        }

        let mut cmd = build_command(&make, &work, &home, BUILD_PROFILE);
        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("emqx-build-spawn-failed: {e}"),
                )
            }
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result.peak_memory_bytes = out.peak_memory_bytes;
        if !out.success {
            result.status = Status::Failed;
            result.reason = Some(BUILD_FAILED.to_string());
            result.notes.push(format!(
                "build output tail: {}",
                tail(&out.stderr, NOTE_TAIL_CHARS)
            ));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-cleaning temp directory unique to this test thread.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "bench-emqx-{}-{}-{:?}",
                std::process::id(),
                tag,
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Scratch { dir }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn metadata_is_stable() {
        let w = Emqx;
        assert_eq!(w.id(), "emqx");
        assert_eq!(w.category(), Category::Messaging);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
        assert_eq!(w.contracts(), &["A.1", "A.9"]);
    }

    #[test]
    fn probe_reports_mix_absence() {
        let p = Emqx.probe();
        if repo().is_dir() && is_emqx_project(&repo()) && !tool::available(MIX) {
            assert_eq!(p.reason(), Some(MIX_ABSENT));
        }
    }

    /// The project-shape guard: a directory without a root `mix.exs` is not an EMQX
    /// umbrella, so its `mix`/Makefile build entry points do not exist.
    #[test]
    fn is_emqx_project_requires_a_root_mix_exs() {
        let s = Scratch::new("project");
        assert!(
            !is_emqx_project(&s.dir),
            "an empty directory is not an EMQX project"
        );

        std::fs::write(s.dir.join("mix.exs"), "defmodule Emqx.MixProject do end\n")
            .expect("write mix.exs");
        assert!(
            is_emqx_project(&s.dir),
            "a root mix.exs makes it an EMQX project"
        );
    }

    #[test]
    fn leading_major_reads_the_leading_integer() {
        assert_eq!(leading_major("28.5.0.7-2"), Some(28));
        assert_eq!(leading_major(" 29"), Some(29));
        assert_eq!(leading_major("6.2-2"), Some(6));
        assert_eq!(leading_major("v28"), None);
        assert_eq!(leading_major(""), None);
    }

    #[test]
    fn required_otp_major_prefers_env_sh_then_tool_versions() {
        let s = Scratch::new("otp");
        // Neither file: nothing declared.
        assert_eq!(required_otp_major(&s.dir), None);

        // `.tool-versions` alone.
        std::fs::write(
            s.dir.join(".tool-versions"),
            "erlang 27.3.1-1\nelixir 1.18\n",
        )
        .expect("write .tool-versions");
        assert_eq!(required_otp_major(&s.dir), Some(27));

        // `env.sh` wins over `.tool-versions`.
        std::fs::write(
            s.dir.join("env.sh"),
            "# c\nset -eu\nexport OTP_VSN=28.5.0.7-2\nexport ELIXIR_VSN=1.19.1\n",
        )
        .expect("write env.sh");
        assert_eq!(required_otp_major(&s.dir), Some(28));
    }

    #[test]
    fn otp_gate_only_trips_on_a_newer_host() {
        assert_eq!(
            otp_gate_reason(28, 29).as_deref(),
            Some("emqx-requires-otp-28")
        );
        assert_eq!(
            otp_gate_reason(28, 30).as_deref(),
            Some("emqx-requires-otp-28")
        );
        assert_eq!(otp_gate_reason(28, 28), None, "a matching host builds");
        assert_eq!(otp_gate_reason(28, 27), None, "an older host is attempted");
        // The note names both sides of the mismatch and the observed mechanism.
        let note = otp_gate_note(28, 29);
        assert!(note.contains("OTP 28") && note.contains("OTP 29"), "{note}");
        assert!(note.contains("env.mk"), "{note}");
    }

    #[test]
    fn toolchain_gate_is_silent_when_the_checkout_declares_nothing() {
        let s = Scratch::new("gate-none");
        assert!(toolchain_gate(&s.dir).is_none());
    }

    #[test]
    fn build_command_drives_the_real_profile_and_redirects_the_home() {
        let make = Path::new("/usr/bin/make");
        let work = Path::new("/tmp/work");
        let home = Path::new("/tmp/home");
        let cmd = build_command(make, work, home, BUILD_PROFILE);

        assert_eq!(cmd.get_program(), make);
        assert_eq!(cmd.get_current_dir(), Some(work));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["emqx-enterprise-compile"]);

        let env: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert!(
            env.contains(&("HOME".to_string(), "/tmp/home".to_string())),
            "{env:?}"
        );
        assert!(
            env.contains(&("MIX_HOME".to_string(), "/tmp/home/.mix".to_string())),
            "{env:?}"
        );
        assert!(
            env.contains(&("HEX_HOME".to_string(), "/tmp/home/.hex".to_string())),
            "{env:?}"
        );
    }

    #[test]
    fn tail_keeps_the_end_and_respects_char_boundaries() {
        assert_eq!(tail("  hello  ", 32), "hello");
        assert_eq!(tail("abcdef", 3), "def");
        // Multi-byte characters must not be split: `aäöü` is 7 bytes, so a 5-byte window
        // would cut `ä` in half and moves back to the nearest boundary instead.
        assert_eq!(tail("aäöü", 5), "öü");
        assert_eq!(tail("aäöü", 4), "öü");
    }
}
