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

//! Oracle resolution and process plumbing (mirrors `tools/differential`).
//!
//! Oracle resolution is `$ERTS_ORACLE` if executable, else `erl` on `$PATH`; when
//! neither exists the calling test prints a loud skip note and returns — a skip,
//! never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn is_executable(p: &Path) -> bool {
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

fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec property] $ERTS_ORACLE='{}' is not an existing executable.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Returns the oracle, or prints a loud skip note and returns `None`.
pub fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("============ beam_codec property: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — install Erlang/OTP and re-run.");
            eprintln!("==============================================================");
            eprintln!();
            None
        }
    }
}

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its raw stdout.
pub fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
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
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval (head) ---\n{}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        &eval[..eval.len().min(400)],
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Parse the `[<<131,...>>,<<...>>]` list the oracle prints into byte vectors.
pub fn parse_binary_list(out: &str) -> Vec<Vec<u8>> {
    let bytes = out.as_bytes();
    let mut images = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'>' && bytes[j + 1] == b'>') {
                j += 1;
            }
            assert!(j + 1 < bytes.len(), "unterminated <<...>> in {out:?}");
            let body = &out[start..j];
            let image = if body.trim().is_empty() {
                Vec::new()
            } else {
                body.split(',')
                    .map(|t| {
                        t.trim()
                            .parse::<u8>()
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in the oracle output"))
                    })
                    .collect()
            };
            images.push(image);
            i = j + 2;
        } else {
            i += 1;
        }
    }
    images
}

/// The `[1,2,3]` textual byte-list form an Erlang `-eval` expression accepts.
pub fn erl_bytes(bytes: &[u8]) -> String {
    let inner = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

/// Index ranges whose rendered/byte `-eval` payload stays under `budget` characters.
pub fn chunks<T: AsRef<str>>(items: &[T], budget: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let mut end = start;
        let mut len = 0;
        while end < items.len() && (end == start || len + items[end].as_ref().len() <= budget) {
            len += items[end].as_ref().len();
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
    out
}
