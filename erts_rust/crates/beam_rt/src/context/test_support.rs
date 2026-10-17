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

//! Shared oracle/fixture plumbing for the `beam_rt` compiled-module tests.
//!
//! Two tests prove the `NEW_FUN_EXT` (`112`) identity derivation against the **system**
//! Erlang: [`super::bridges`]'s derivation-level test (identity + explicit
//! [`beam_codec::Encoder`]) and [`super::fun_identity`]'s end-to-end `term_to_binary/1`
//! test (through the real BIF dispatch). Both need the same toolchain discovery, fixture
//! compilation, oracle invocation and hex codec, so those live here once instead of being
//! copy-pasted (workspace `CONTEXT.md`: no duplication across — or within — crates).
//!
//! The module is `#[cfg(test)]`, so nothing here ships. A missing `erl`/`erlc` is reported
//! through [`skip_loudly`] — a loud skip banner, never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether `p` is an existing executable file.
pub(super) fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The first `name` on `$PATH` that is an executable file.
pub(super) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Resolve the oracle the way `tools/differential` does: `$ERTS_ORACLE` if executable,
/// else `erl` on `$PATH`.
pub(super) fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        return is_executable(&program).then_some(program);
    }
    find_on_path("erl")
}

/// The compiler: a sibling of the oracle first (the same installation), then `$PATH`.
pub(super) fn resolve_erlc() -> Option<PathBuf> {
    if let Some(oracle) = resolve_oracle() {
        if let Some(dir) = oracle.parent() {
            let candidate = dir.join("erlc");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    find_on_path("erlc")
}

/// Prints the loud skip banner — a missing toolchain is never a silent pass.
pub(super) fn skip_loudly(banner: &str, why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt {banner} SKIPPED ===============");
    eprintln!("{why}: the compiled-module NEW_FUN_EXT proof cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("===============================================================");
    eprintln!();
}

/// Monotonic per-call sequence number making each fixture scratch directory unique even
/// within one process (the tests run in parallel threads of the same test binary).
pub(super) static FIXTURE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// The per-process nonce every scratch-directory name carries, computed **once** on first
/// use (`OnceLock`), so it is identical for every call and every thread of one OS process.
///
/// The pid alone cannot identify the process: the test sandbox runs each agent's shell in
/// its own PID namespace while sharing one `$TMPDIR`, so two concurrent `beam_rt` test
/// binaries can carry the *same* namespace-local pid — and then the *same* `FIXTURE_SEQ`
/// value — and one process's [`FixtureDir::drop`] would delete the directory the other is
/// still compiling into. Mixing three std-only entropy sources, none of which collides
/// across live processes, makes the name unique without a new dependency:
///
/// * the process start instant (the wall clock at the first call, captured once),
/// * the pid (still useful inside one namespace),
/// * the address of this very static (ASLR entropy of a PIE image).
///
/// The mix is a splitmix64-style avalanche so no single source's low bits dominate the name.
pub(super) fn process_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    *NONCE.get_or_init(|| {
        // A clock before the epoch is pathological for a test host; fall back to 0 rather
        // than fail — the pid and address entropy still mix in.
        let start_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let address = std::ptr::addr_of!(NONCE) as usize as u64;
        let mut x = start_nanos ^ address.rotate_left(17) ^ (u64::from(std::process::id()) << 32);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    })
}

/// A compiled-fixture scratch directory that removes itself on drop, so the per-call
/// unique directories never accumulate in the temp dir.
pub(crate) struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    /// The directory itself (e.g. for `erlc -o` / the oracle's `-pa`).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory (e.g. the compiled `.beam`).
    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test,
        // yet a cleanup failure is reported rather than swallowed.
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "warning: could not remove the fixture scratch dir {}: {e}",
                self.path.display()
            );
        }
    }
}

/// Compiles `source` (Erlang module `module`) with `erlc` into a scratch directory unique
/// **per call** — keyed by `label`, the process id, an atomic counter and the per-process
/// [`process_nonce`], so parallel tests never race on one directory (not even across test
/// binaries whose namespace-local pids collide) — and returns it.
///
/// The module name doubles as the source file stem (`<module>.erl`) and the expected
/// `<module>.beam`, matching what `erlc` produces.
pub(super) fn compile_fixture(
    erlc: &Path,
    label: &str,
    module: &str,
    source: &str,
) -> FixtureDir {
    compile_fixtures(erlc, label, &[(module, source)])
}

/// A fresh, **empty** scratch directory that removes itself on drop.
///
/// Same per-call uniqueness (label/pid/atomic counter/process nonce) and same self-removal
/// contract as [`compile_fixture`], for a test that needs a directory of its own rather than
/// compiled fixtures — e.g. the `{outdir, Dir}` of an in-process `compile:file/2` (the compiler
/// does not create it), a scratch `-pa` for the oracle, or a scratch `-pa` holding a deliberately
/// corrupt `.beam`. Every test that needs a scratch directory uses it, in every node of the
/// crate, so the uniqueness scheme exists once.
pub(crate) fn scratch_dir(label: &str) -> FixtureDir {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_scratch_{label}_{pid}_{seq}_{nonce:016x}",
        pid = std::process::id(),
        nonce = process_nonce(),
    ));
    std::fs::create_dir_all(&dir).expect("the test temp dir is writable");
    FixtureDir { path: dir }
}

/// Compiles several `(module, source)` pairs with `erlc` into **one** scratch directory, so a
/// test can load (or let the oracle load) modules that call into each other — e.g. a fun built
/// in one module and encoded from another.
///
/// The directory is unique per call (the same label/pid/atomic-counter/process-nonce scheme
/// [`compile_fixture`] uses) and removes itself on drop.
pub(super) fn compile_fixtures(
    erlc: &Path,
    label: &str,
    modules: &[(&str, &str)],
) -> FixtureDir {
    let dir = scratch_dir(label);
    for (module, source) in modules {
        let src = dir.path.join(format!("{module}.erl"));
        std::fs::write(&src, source).expect("the fixture source is writable");
        let output = Command::new(erlc)
            .arg("-o")
            .arg(&dir.path)
            .arg(&src)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn erlc '{}': {e}", erlc.display()));
        assert!(
            output.status.success(),
            "erlc '{}' failed on {module} with {:?}\n--- stderr ---\n{}",
            erlc.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    dir
}

/// Runs `eval` under the oracle with `code_dir` prepended to the code path and returns its
/// stdout, one trimmed, lowercased, non-empty line at a time.
pub(super) fn run_oracle_probe(oracle: &Path, code_dir: &Path, eval: &str) -> Vec<String> {
    run_oracle_probe_exact(oracle, code_dir, eval)
        .into_iter()
        .map(|line| line.to_lowercase())
        .collect()
}

/// Like [`run_oracle_probe`], but returns each stdout line **trimmed only** — the letter case
/// is preserved, so a comparison can tell `'-all/0-Fact/1-0-'` from a lowercased spelling
/// (a lambda's generated name, which `fun_info(F, name)` reports verbatim).
pub(super) fn run_oracle_probe_exact(oracle: &Path, code_dir: &Path, eval: &str) -> Vec<String> {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-pa")
        .arg(code_dir)
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"));
    stdout
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Decodes an even-length hex line into bytes.
pub(super) fn decode_hex(line: &str) -> Vec<u8> {
    assert!(
        line.chars().all(|c| c.is_ascii_hexdigit()),
        "not a hex line: {line:?}"
    );
    assert_eq!(line.len() % 2, 0, "odd-length hex line: {line:?}");
    (0..line.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&line[i..i + 2], 16)
                .unwrap_or_else(|e| panic!("bad hex pair in {line:?}: {e}"))
        })
        .collect()
}

/// An uppercase hex rendering of `bytes` for diagnostics.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two calls with one label must own two distinct directories, and each name must carry
    /// the per-process [`process_nonce`] — the pid and the counter alone can repeat in another
    /// PID namespace sharing this `$TMPDIR`, and the loser's drop would delete a directory a
    /// concurrent test binary is still compiling into.
    #[test]
    fn scratch_dir_names_are_unique_per_call_and_carry_the_process_nonce() {
        let nonce = process_nonce();
        assert_eq!(process_nonce(), nonce, "the nonce is fixed for the process");

        let first = scratch_dir("nonce_shape");
        let second = scratch_dir("nonce_shape");
        assert_ne!(
            first.path(),
            second.path(),
            "two calls with the same label yield two distinct directories"
        );

        let name = first
            .path()
            .file_name()
            .expect("the scratch path has a file name")
            .to_string_lossy()
            .into_owned();
        let without_nonce = name
            .strip_suffix(&format!("_{nonce:016x}"))
            .unwrap_or_else(|| panic!("{name} must carry the nonce {nonce:016x}"));
        let seq = without_nonce
            .strip_prefix(&format!(
                "beam_rt_scratch_nonce_shape_{}_",
                std::process::id()
            ))
            .unwrap_or_else(|| panic!("{name} must keep the label/pid prefix"));
        assert!(
            seq.parse::<usize>().is_ok(),
            "the seq part stays numeric: {seq:?}"
        );
    }

    /// `scratch_dir` still creates the directory and `FixtureDir` still removes exactly that
    /// path on drop.
    #[test]
    fn a_fixture_dir_removes_exactly_the_directory_it_created() {
        let dir = scratch_dir("drop_shape");
        let path = dir.path().to_path_buf();
        assert!(path.is_dir(), "scratch_dir creates the directory");
        drop(dir);
        assert!(!path.exists(), "drop removes the directory it created");
    }
}
