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

//! The `bif.tab` surface: a machine-checkable statement of the real ERTS BIF roster.
//!
//! Compatibility contract Part I §A.7 pins the BIF surface to the *names, modules and
//! arities* of the real table `../../erts/emulator/beam/bif.tab`. [`crate::registry`]
//! is a hand-written roster of `fn` pointers (the registry is deliberately **not**
//! data-driven: a BIF is a Rust function, registered by its own module), so nothing
//! in the build previously tied that roster to the source data. This module closes
//! that gap: it parses the real table and computes a [`Census`] of what the registry
//! implements against what `bif.tab` declares.
//!
//! ## The real file's grammar (read off the file, not assumed)
//!
//! `bif.tab` documents its own format at the top. The checkout **only** uses:
//!
//! ```text
//! <bif-decl> ::= ("bif" | "ubif" | "hbif") <module> ":" <name> "/" <arity> <C-name>*
//! ```
//!
//! * `#` starts a **whole-line** comment (there are no inline comments in this file);
//! * blank lines are ignored;
//! * `name` may be a quoted operator (`` 'and' ``, `` '+' ``, `` '=:=' ``, `` '!' ``, `` '/' ``), kept
//!   **verbatim including the quotes** — that is exactly how the registry keys them;
//! * the trailing `C-name` column (e.g. `error_1`, `binary_binary_part_2`) is irrelevant
//!   to the `(module, name, arity)` identity and is ignored;
//! * `bif`/`ubif`/`hbif` differ only in how the C loader dispatches them; all three carry
//!   the same identity, so all three are parsed and kept distinct by [`BifKind`].
//!
//! Two grammar features the objective *expected* but that this file does **not** contain
//! are handled defensively: an arity **range** (`name/1-2`, expanded to `1` and `2`;
//! no such entry occurs here, so the parser is exercised on a synthetic fixture) is
//! supported, while `?MODULE` substitution and standalone `C_`/`#`/`-` directives do
//! not occur and are therefore not invented — an unknown directive is a parse error.
//!
//! ## Failure is return-based (workspace `CONTEXT.md` §3)
//!
//! Parsing source data is not a runtime BIF failure, so it does **not** report a
//! [`beam_common::BeamError`]; it returns a [`BifTabError`] carrying the offending line.
//! A malformed table is a bug in the *validation* tool, never a silently-ignored line.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::registry::BifRegistry;

/// The three declaration keywords of `bif.tab`.
///
/// `ubif` marks operators and guard BIFs, `hbif` BIFs that must be called through the
/// export entry (they GC or need the call site); `bif` is everything else. The
/// distinction is a loader detail — all three carry the same `(module, name, arity)`
/// identity — but it is preserved so the census can report per-kind coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BifKind {
    /// `bif` — an ordinary BIF.
    Bif,
    /// `ubif` — an operator or guard BIF.
    Ubif,
    /// `hbif` — a BIF that must be called through the export entry.
    Hbif,
}

impl BifKind {
    /// The literal keyword as it appears in `bif.tab`.
    pub fn keyword(self) -> &'static str {
        match self {
            BifKind::Bif => "bif",
            BifKind::Ubif => "ubif",
            BifKind::Hbif => "hbif",
        }
    }
}

/// One `<module>:<name>/<arity>` declaration from `bif.tab`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BifTabEntry {
    /// The declaration keyword.
    pub kind: BifKind,
    /// The module, verbatim (`erlang`, `erts_internal`, `records`, …).
    pub module: String,
    /// The function name, verbatim **including** the quotes of a quoted operator.
    pub name: String,
    /// The arity.
    pub arity: u32,
}

impl BifTabEntry {
    /// The registry key this entry names.
    pub fn key(&self) -> (&str, &str, u32) {
        (self.module.as_str(), self.name.as_str(), self.arity)
    }

    /// An owned copy of [`BifTabEntry::key`], for set membership.
    pub fn owned_key(&self) -> (String, String, u32) {
        (self.module.clone(), self.name.clone(), self.arity)
    }

    /// The sort/display key `module:name/arity`, spelled exactly as in `bif.tab`.
    pub fn spec(&self) -> String {
        format!("{}:{}/{}", self.module, self.name, self.arity)
    }
}

/// A parse failure in `bif.tab`, carrying the 1-based source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BifTabError {
    /// The 1-based line number in the parsed text.
    pub line: usize,
    /// The human-readable reason.
    pub message: String,
}

impl BifTabError {
    fn new(line: usize, message: impl Into<String>) -> BifTabError {
        BifTabError {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for BifTabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bif.tab:{}: {}", self.line, self.message)
    }
}

impl std::error::Error for BifTabError {}

/// A parsed `bif.tab`.
#[derive(Debug, Clone, Default)]
pub struct BifTab {
    entries: Vec<BifTabEntry>,
    index: BTreeSet<(String, String, u32)>,
}

impl BifTab {
    /// Parses `bif.tab` text (the real file, or a fixture).
    ///
    /// # Errors
    ///
    /// Any line that is neither blank, a `#` comment, nor a well-formed
    /// `bif`/`ubif`/`hbif` declaration is [`BifTabError`]; a duplicated
    /// `(module, name, arity)` is also an error (the real table has none, so a
    /// duplicate means the table was corrupted).
    pub fn parse(text: &str) -> Result<BifTab, BifTabError> {
        let mut tab = BifTab::default();
        for (offset, raw) in text.lines().enumerate() {
            let line = offset + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut tokens = trimmed.split_whitespace();
            let directive = tokens.next().expect("a non-empty line has a first token");
            let kind = match directive {
                "bif" => BifKind::Bif,
                "ubif" => BifKind::Ubif,
                "hbif" => BifKind::Hbif,
                other => {
                    return Err(BifTabError::new(
                        line,
                        format!(
                            "unknown directive `{other}` (expected `bif`, `ubif` or `hbif`)"
                        ),
                    ))
                }
            };
            let spec = tokens.next().ok_or_else(|| {
                BifTabError::new(line, "missing the `module:name/arity` specification")
            })?;
            // The remaining tokens are the optional C names (`error_1`, …): they do
            // not affect the identity of the BIF, so they are consumed and dropped.
            let _c_names: Vec<&str> = tokens.collect();

            for (module, name, arity) in parse_spec(line, spec)? {
                let entry = BifTabEntry {
                    kind,
                    module,
                    name,
                    arity,
                };
                if !tab.index.insert(entry.owned_key()) {
                    return Err(BifTabError::new(
                        line,
                        format!("duplicate declaration of `{}`", entry.spec()),
                    ));
                }
                tab.entries.push(entry);
            }
        }
        Ok(tab)
    }

    /// Every entry, in file order.
    pub fn entries(&self) -> &[BifTabEntry] {
        &self.entries
    }

    /// The number of declared BIFs (a `name/1-2` range counts as its expansion).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table declared nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of declarations of `kind`.
    pub fn count(&self, kind: BifKind) -> usize {
        self.entries.iter().filter(|e| e.kind == kind).count()
    }

    /// The declared entry for `(module, name, arity)`, if any.
    pub fn get(&self, module: &str, name: &str, arity: u32) -> Option<&BifTabEntry> {
        if !self
            .index
            .contains(&(module.to_string(), name.to_string(), arity))
        {
            return None;
        }
        self.entries
            .iter()
            .find(|e| e.key() == (module, name, arity))
    }

    /// Whether `(module, name, arity)` is declared.
    pub fn contains(&self, module: &str, name: &str, arity: u32) -> bool {
        self.index
            .contains(&(module.to_string(), name.to_string(), arity))
    }
}

/// Splits `module:name/arity` (or `module:name/lo-hi`) into entries.
fn parse_spec(line: usize, spec: &str) -> Result<Vec<(String, String, u32)>, BifTabError> {
    let (module, rest) = spec.split_once(':').ok_or_else(|| {
        BifTabError::new(line, format!("`{spec}` is not `module:name/arity` (no `:`)`"))
    })?;
    if module.is_empty() {
        return Err(BifTabError::new(
            line,
            format!("`{spec}` has an empty module name"),
        ));
    }
    // `rsplit_once`: an operator name may itself contain `/` (`erlang:'/'/2`).
    let (name, arity_part) = rest.rsplit_once('/').ok_or_else(|| {
        BifTabError::new(line, format!("`{spec}` is missing the `/arity` suffix"))
    })?;
    if name.is_empty() {
        return Err(BifTabError::new(
            line,
            format!("`{spec}` has an empty function name"),
        ));
    }
    // An operator or keyword is written quoted (`'and'`, `'+'`, `'=:='`, `'!'`,
    // `'/'`); the quotes are Erlang term syntax, not part of the atom, so they are
    // stripped to recover the name the registry keys the BIF under.
    let name = unquote(name);
    let arities = parse_arity_range(line, spec, arity_part)?;
    Ok(arities
        .into_iter()
        .map(|arity| (module.to_string(), name.to_string(), arity))
        .collect())
}

/// Strips the surrounding single quotes of a quoted name (`'and'` → `and`).
///
/// A name that is not quoted is returned unchanged.
fn unquote(name: &str) -> &str {
    match name.strip_prefix('\'').and_then(|n| n.strip_suffix('\'')) {
        Some(inner) if !inner.is_empty() => inner,
        _ => name,
    }
}

/// Parses `N` or the inclusive range `lo-hi`.
fn parse_arity_range(line: usize, spec: &str, part: &str) -> Result<Vec<u32>, BifTabError> {
    match part.split_once('-') {
        None => Ok(vec![part.parse::<u32>().map_err(|_| {
            BifTabError::new(line, format!("`{spec}` has a non-numeric arity `{part}`"))
        })?]),
        Some((lo, hi)) => {
            let lo = lo.parse::<u32>().map_err(|_| {
                BifTabError::new(line, format!("`{spec}` has a non-numeric arity `{lo}`"))
            })?;
            let hi = hi.parse::<u32>().map_err(|_| {
                BifTabError::new(line, format!("`{spec}` has a non-numeric arity `{hi}`"))
            })?;
            if lo > hi {
                return Err(BifTabError::new(
                    line,
                    format!("`{spec}` has a descending arity range"),
                ));
            }
            Ok((lo..=hi).collect())
        }
    }
}

/// The location of the real table in this OTP checkout.
///
/// Resolved against `CARGO_MANIFEST_DIR` (`erts_rust/crates/beam_bifs`), never the
/// process CWD, so it is correct under `cargo test` from anywhere.
pub fn bif_tab_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("erts/emulator/beam/bif.tab")
}

/// Reads the real `bif.tab` text, or `None` when the file is absent.
///
/// `None` happens only when the crate is built outside an OTP source tree (a
/// published crate has no `erts/`); callers are expected to **skip loudly**, never
/// to fail, in that case.
pub fn read_real_bif_tab() -> Option<String> {
    std::fs::read_to_string(bif_tab_path()).ok()
}

/// The coverage census of the Rust registry against a parsed `bif.tab`.
///
/// `implemented` and `missing` together partition the declared table; `implemented`
/// and `fast_paths` together partition the registry. Every vector is sorted.
#[derive(Debug, Clone, Default)]
pub struct Census {
    /// Declared entries the registry implements (key matched exactly).
    pub implemented: Vec<BifTabEntry>,
    /// Declared entries the registry does **not** implement, sorted.
    ///
    /// This is the honest, enumerated gap: it is *reported*, never a failure.
    pub missing: Vec<BifTabEntry>,
    /// Registry keys the table does not declare — the `erlang.erl`/`lists.erl`/…
    /// stdlib fast paths, sorted.
    pub fast_paths: Vec<(String, String, u32)>,
    /// Declared `bif` entries.
    pub declared_bif: usize,
    /// Declared `ubif` entries.
    pub declared_ubif: usize,
    /// Declared `hbif` entries.
    pub declared_hbif: usize,
    /// Implemented declared `bif` entries.
    pub implemented_bif: usize,
    /// Implemented declared `ubif` entries.
    pub implemented_ubif: usize,
    /// Implemented declared `hbif` entries.
    pub implemented_hbif: usize,
}

impl Census {
    /// The number of declared entries (implemented + missing).
    pub fn declared_total(&self) -> usize {
        self.declared_bif + self.declared_ubif + self.declared_hbif
    }

    /// The number of implemented declared entries.
    pub fn implemented_total(&self) -> usize {
        self.implemented_bif + self.implemented_ubif + self.implemented_hbif
    }
    /// The number of declared entries the registry implements.
    pub fn implemented_count(&self) -> usize {
        self.implemented.len()
    }

    /// The number of declared entries the registry does not implement.
    pub fn missing_count(&self) -> usize {
        self.missing.len()
    }

    /// The number of registry keys the table does not declare.
    pub fn fast_path_count(&self) -> usize {
        self.fast_paths.len()
    }

    /// The size of the registry (implemented declared entries + fast paths).
    pub fn registry_total(&self) -> usize {
        self.implemented.len() + self.fast_paths.len()
    }

    /// The missing entries per module, `(module, count)`, sorted by module.
    pub fn missing_by_module(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for entry in &self.missing {
            *counts.entry(entry.module.clone()).or_default() += 1;
        }
        counts.into_iter().collect()
    }

    /// The fast paths per module, `(module, count)`, sorted by module.
    pub fn fast_paths_by_module(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for (module, _, _) in &self.fast_paths {
            *counts.entry(module.clone()).or_default() += 1;
        }
        counts.into_iter().collect()
    }
}

/// Computes the [`Census`] of `reg` against `tab`.
///
/// A registry key is *declared* iff the table declares it with exactly the same
/// module, name (including an operator's quotes) and arity; anything else is a fast
/// path. The registry itself is never consulted for the declared set, so the census
/// cannot be fooled by a typo in the roster — a mistyped BIF shows up as a fast path
/// **and** as a missing entry.
pub fn census(tab: &BifTab, reg: &BifRegistry) -> Census {
    let mut implemented = Vec::new();
    let mut implemented_keys = BTreeSet::new();
    let mut fast_paths = Vec::new();
    for (module, name, arity) in reg.names() {
        match tab.get(&module, &name, arity) {
            Some(entry) => {
                implemented_keys.insert(entry.owned_key());
                implemented.push(entry.clone());
            }
            None => fast_paths.push((module, name, arity)),
        }
    }
    implemented.sort_by(|a, b| a.owned_key().cmp(&b.owned_key()));
    let mut missing: Vec<BifTabEntry> = tab
        .entries()
        .iter()
        .filter(|e| !implemented_keys.contains(&e.owned_key()))
        .cloned()
        .collect();
    missing.sort_by(|a, b| a.owned_key().cmp(&b.owned_key()));
    let count_kind = |kind: BifKind| implemented.iter().filter(|e| e.kind == kind).count();
    Census {
        implemented_bif: count_kind(BifKind::Bif),
        implemented_ubif: count_kind(BifKind::Ubif),
        implemented_hbif: count_kind(BifKind::Hbif),
        implemented,
        missing,
        fast_paths,
        declared_bif: tab.count(BifKind::Bif),
        declared_ubif: tab.count(BifKind::Ubif),
        declared_hbif: tab.count(BifKind::Hbif),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::default_registry;

    #[test]
    fn parses_the_three_directives_and_skips_comments_and_blanks() {
        let text = "\
# a comment

bif erlang:error/1\t\terror_1
ubif erlang:'+'/2\t\tsplus_2
hbif erlang:put/2
bif\terlang:'/'/2
";
        let tab = BifTab::parse(text).expect("well-formed fixture");
        assert_eq!(tab.len(), 4);
        assert_eq!(tab.count(BifKind::Bif), 2);
        assert_eq!(tab.count(BifKind::Ubif), 1);
        assert_eq!(tab.count(BifKind::Hbif), 1);
        assert!(tab.contains("erlang", "error", 1));
        assert!(tab.contains("erlang", "+", 2));
        assert!(tab.contains("erlang", "put", 2));
        // The quoted `/` operator survives `rsplit_once`.
        assert!(tab.contains("erlang", "/", 2));
        assert_eq!(tab.get("erlang", "/", 2).unwrap().kind, BifKind::Bif);
    }

    #[test]
    fn expands_an_arity_range() {
        // No such entry occurs in the real table; the grammar support is pinned
        // here so a future `name/lo-hi` line cannot silently mis-parse.
        let tab = BifTab::parse("bif erlang:foo/1-2\n").expect("a range is well-formed");
        assert_eq!(tab.len(), 2);
        assert!(tab.contains("erlang", "foo", 1));
        assert!(tab.contains("erlang", "foo", 2));
        assert!(!tab.contains("erlang", "foo", 3));
        // A single-arity line is a unit range.
        let one = BifTab::parse("ubif erlang:bar/1-1\n").expect("a unit range");
        assert_eq!(one.len(), 1);
        assert_eq!(one.entries()[0].kind, BifKind::Ubif);
    }

    #[test]
    fn rejects_malformed_lines() {
        for (text, needle) in [
            ("bif erlang:error/1\nbogus erlang:x/1\n", "unknown directive"),
            ("bif erlangerror/1\n", "no `:`"),
            ("bif erlang:error/\n", "non-numeric arity"),
            ("bif :error/1\n", "empty module"),
            ("bif erlang:/1\n", "empty function name"),
            ("bif erlang:error/x\n", "non-numeric arity"),
            ("bif erlang:error/2-1\n", "descending arity range"),
            ("ubif\n", "missing the `module:name/arity`"),
            ("bif erlang:error/1\nbif erlang:error/1\n", "duplicate"),
        ] {
            let err = BifTab::parse(text).expect_err(needle);
            assert!(
                err.message.contains(needle),
                "expected `{needle}` in `{err}`"
            );
        }
        // The error points at the offending line.
        let err = BifTab::parse("# c\nbif erlang:a/1\nbogus\n").expect_err("a bad line");
        assert_eq!(err.line, 3);
        // A duplicate reports the *second* line.
        let err = BifTab::parse("bif erlang:a/1\n\nbif erlang:a/1\n").expect_err("a duplicate");
        assert_eq!(err.line, 3);
    }

    #[test]
    fn the_real_table_parses_with_the_expected_shape() {
        let Some(text) = read_real_bif_tab() else {
            eprintln!("SKIP: erts/emulator/beam/bif.tab is not present in this checkout");
            return;
        };
        let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
        assert_eq!(tab.count(BifKind::Bif), 476);
        assert_eq!(tab.count(BifKind::Ubif), 73);
        assert_eq!(tab.count(BifKind::Hbif), 7);
        assert_eq!(tab.len(), 556);
        // Sanity: a few entries, including the quoted operators.
        assert!(tab.contains("erlang", "+", 2));
        assert!(tab.contains("erlang", "/", 2));
        assert!(tab.contains("erlang", "!", 2));
        assert!(tab.contains("erlang", "++", 2));
        assert!(tab.contains("records", "create", 4));
        assert!(tab.contains("erts_internal", "cmp_term", 2));
        assert!(!tab.contains("erlang", "not_a_bif", 1));
    }

    #[test]
    fn census_partitions_the_table_and_the_registry() {
        let Some(text) = read_real_bif_tab() else {
            eprintln!("SKIP: erts/emulator/beam/bif.tab is not present in this checkout");
            return;
        };
        let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
        let reg = default_registry();
        let census = census(&tab, &reg);

        // The declared table is partitioned by implemented ⊎ missing.
        assert_eq!(
            census.implemented_count() + census.missing_count(),
            census.declared_total()
        );
        // The per-kind counters agree with the enumerated implemented list.
        assert_eq!(census.implemented_total(), census.implemented_count());
        assert_eq!(
            census.implemented_bif + census.implemented_ubif + census.implemented_hbif,
            census.implemented_count()
        );
        // The registry is partitioned by declared-implemented ⊎ fast paths.
        assert_eq!(census.registry_total(), reg.len());
        // No key can be both implemented and missing.
        for entry in &census.implemented {
            assert!(!census
                .missing
                .iter()
                .any(|m| m.owned_key() == entry.owned_key()));
        }
        // Every implemented entry really is declared.
        for entry in &census.implemented {
            assert!(
                tab.contains(entry.module.as_str(), entry.name.as_str(), entry.arity),
                "`{}` is not declared",
                entry.spec()
            );
        }
        // Every fast path is genuinely absent from the table.
        for (module, name, arity) in &census.fast_paths {
            assert!(!tab.contains(module, name, *arity));
        }
    }
}
