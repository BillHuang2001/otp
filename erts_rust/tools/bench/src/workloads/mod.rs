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

//! The built-in workload registry and the shared helpers they use.
//!
//! Each workload lives in its own module; this file wires them into the [`registry`]
//! and hosts the small, dependency-free helpers (repo resolution, recursive file
//! collection, line counting, external-tool resolution via [`tool`]) that several
//! workloads share.

use std::path::{Path, PathBuf};

use crate::workload::Workload;

pub mod baseline;
pub mod binary_append;
pub mod copy_bound;
pub mod cowboy;
pub mod elixir_compiler;
pub mod emqx;
pub mod erlc_compiler;
pub mod foreign;
pub mod foreign_sample;
pub mod gc_bound;
pub mod khepri;
pub mod list_walk;
pub mod plb;
pub mod slice;
pub mod tool;
/// The workload ids that accept a `--repo <id>=<path>` override.
pub const REPO_WORKLOADS: [&str; 5] = ["elixir_compiler", "plb", "cowboy", "khepri", "emqx"];

/// The environment variable name that overrides workload `id`'s external repo path.
pub fn repo_env_var(id: &str) -> String {
    format!("BENCH_{}_REPO", id.to_ascii_uppercase().replace('-', "_"))
}

/// Applies a `--repo <id>=<path>` override to the process environment.
///
/// The override is stored in the same `BENCH_<ID>_REPO` variable that
/// [`resolve_repo`] reads, so the resolution logic has a single code path.
pub fn set_repo_override(id: &str, path: &Path) {
    std::env::set_var(repo_env_var(id), path);
}

/// Resolves an external repo root: `$BENCH_<ID>_REPO` if set and non-empty, else the
/// documented default.
pub fn resolve_repo(id: &str, default: &str) -> PathBuf {
    match std::env::var_os(repo_env_var(id)) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(default),
    }
}

/// Recursively collects files under `dir` whose extension equals `ext`, sorted for
/// determinism. Missing/unreadable directories yield an empty vector.
pub fn collect_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_into(dir, ext, &mut out);
    out.sort();
    out
}

fn collect_into(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_into(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

/// Counts the total number of lines across `paths` (a LOC proxy). Unreadable files
/// count as zero.
pub fn count_source_lines(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .map(|p| {
            std::fs::read_to_string(p)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        })
        .sum()
}

/// The directory at `<CARGO_MANIFEST_DIR>/../../..`, i.e. `$ERL_TOP` for this checkout.
pub fn erl_top() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The default `out/` directory of this crate (`<CARGO_MANIFEST_DIR>/out`).
pub fn default_out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("out")
}

/// Assembles the built-in workload registry, in listing order.
pub fn registry() -> Vec<Box<dyn Workload>> {
    vec![
        Box::new(baseline::Baseline),
        Box::new(erlc_compiler::ErlcCompiler),
        Box::new(elixir_compiler::ElixirCompiler),
        Box::new(plb::Plb),
        Box::new(cowboy::Cowboy),
        Box::new(khepri::Khepri),
        Box::new(emqx::Emqx),
        Box::new(slice::Slice),
        Box::new(foreign::Foreign),
        Box::new(list_walk::ListWalk),
        Box::new(copy_bound::CopyBound),
        Box::new(gc_bound::GcBound),
        Box::new(binary_append::BinaryAppend),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_env_var_names_are_uppercased() {
        assert_eq!(repo_env_var("cowboy"), "BENCH_COWBOY_REPO");
        assert_eq!(repo_env_var("elixir_compiler"), "BENCH_ELIXIR_COMPILER_REPO");
        assert_eq!(repo_env_var("plb"), "BENCH_PLB_REPO");
    }

    #[test]
    fn resolve_repo_falls_back_to_default() {
        // These ids are never set in the test environment's BENCH_* namespace.
        let p = resolve_repo("definitely_unset_id_xyzzy", "/fallback/here");
        assert_eq!(p, PathBuf::from("/fallback/here"));
    }

    #[test]
    fn collect_files_is_recursive_sorted_and_filtered() {
        let base = std::env::temp_dir().join(format!("bench-collect-{}", std::process::id()));
        let nested = base.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(base.join("b.erl"), "x\n").unwrap();
        std::fs::write(base.join("a.erl"), "x\n").unwrap();
        std::fs::write(nested.join("c.erl"), "x\n").unwrap();
        std::fs::write(base.join("ignored.txt"), "x\n").unwrap();

        let files = collect_files(&base, "erl");
        assert_eq!(files.len(), 3);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.erl", "b.erl", "c.erl"]);

        assert!(collect_files(&base.join("missing"), "erl").is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn count_source_lines_sums_files() {
        assert_eq!(count_source_lines(&[]), 0);
        let base = std::env::temp_dir().join(format!("bench-loc-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let f = base.join("x.erl");
        std::fs::write(&f, "1\n2\n3\n").unwrap();
        assert_eq!(count_source_lines(&[f.clone()]), 3);
        assert_eq!(count_source_lines(&[base.join("missing")]), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn erl_top_points_at_the_repo_root() {
        assert!(erl_top().is_dir());
        assert!(erl_top().join("lib/stdlib/src").is_dir());
    }

    #[test]
    fn registry_has_all_documented_workloads() {
        let ids: Vec<&str> = registry().iter().map(|w| w.id()).collect();
        assert_eq!(
            ids,
            vec![
                "baseline",
                "erlc_compiler",
                "elixir_compiler",
                "plb",
                "cowboy",
                "khepri",
                "emqx",
                "slice",
                "foreign",
                "list_walk",
                "copy_bound",
                "gc_bound",
                "binary_append"
            ]
        );
    }
}
