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

//! The known-gaps **allowlist**: the committed record of every capability gap the
//! compatibility gate currently tolerates.
//!
//! This is the ratchet that lets `suite --strict --allowlist <file>` PASS while a set of
//! documented gaps exists, and FAIL — so the allowlist must be updated — the moment an
//! *unauthorized* gap appears or an allowlist entry goes *stale* (its case no longer a
//! gap). The parse is deliberately simple and line-based: the file is source data a human
//! reviews, not a machine-generated artifact.
//!
//! ```text
//! # comments and blank lines are ignored
//! <case-id> <owning-crate> <reason…>
//! ```
//!
//! A read failure is a loud [`Error::Io`] (exit `2`); a malformed or duplicate entry is a
//! loud [`Error::Usage`] (exit `2`) naming the 1-based line.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// One allowlisted capability gap: the corpus case, the crate that must close it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowEntry {
    /// The corpus case id (`<group>/<name>`).
    pub case_id: String,
    /// The owning crate (the node whose gap this is).
    pub crate_name: String,
    /// A free-form human reason for tolerating the gap.
    pub reason: String,
}

/// The parsed known-gaps allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowlist {
    /// Where the allowlist was loaded from (`None` for an in-memory list).
    path: Option<PathBuf>,
    /// The entries, in file order.
    entries: Vec<AllowEntry>,
}

impl Allowlist {
    /// Parse the allowlist text.
    ///
    /// `#`-comment lines and blank lines are ignored; the leading/trailing whitespace and a
    /// trailing `\r` are trimmed. Every other line is
    /// `<case-id> <owning-crate> <reason…>`. Fewer than three whitespace-separated tokens is
    /// a usage error, as is a duplicate case id.
    pub fn parse(source: &str, path: Option<PathBuf>) -> Result<Allowlist> {
        let mut entries: Vec<AllowEntry> = Vec::new();
        for (index, raw) in source.lines().enumerate() {
            let line_number = index + 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw).trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() < 3 {
                return Err(Error::usage(format!(
                    "malformed allowlist entry on line {line_number}: expected \
                     `<case-id> <owning-crate> <reason>`"
                )));
            }
            let case_id = tokens[0].to_string();
            let crate_name = tokens[1].to_string();
            let reason = tokens[2..].join(" ");
            if entries.iter().any(|entry| entry.case_id == case_id) {
                return Err(Error::usage(format!(
                    "duplicate allowlist entry for '{case_id}' on line {line_number}"
                )));
            }
            entries.push(AllowEntry {
                case_id,
                crate_name,
                reason,
            });
        }
        Ok(Allowlist { path, entries })
    }

    /// Read and parse an allowlist file.
    ///
    /// A read failure is a loud [`Error::Io`] naming the path.
    pub fn load(path: &Path) -> Result<Allowlist> {
        let source = std::fs::read_to_string(path).map_err(|source| {
            Error::io(
                format!("reading the allowlist '{}'", path.display()),
                source,
            )
        })?;
        Allowlist::parse(&source, Some(path.to_path_buf()))
    }

    /// An empty in-memory allowlist (`path == None`).
    pub fn empty() -> Allowlist {
        Allowlist {
            path: None,
            entries: Vec::new(),
        }
    }

    /// Where this allowlist came from, if anywhere.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The entries, in file order.
    pub fn entries(&self) -> &[AllowEntry] {
        &self.entries
    }

    /// How many entries the allowlist carries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the allowlist carries no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry for `case_id`, if one is recorded.
    pub fn get(&self, case_id: &str) -> Option<&AllowEntry> {
        self.entries.iter().find(|entry| entry.case_id == case_id)
    }

    /// Whether `case_id` is recorded in the allowlist.
    pub fn contains(&self, case_id: &str) -> bool {
        self.get(case_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{AllowEntry, Allowlist};
    use crate::error::Error;
    use std::path::{Path, PathBuf};

    fn entry(case_id: &str, crate_name: &str, reason: &str) -> AllowEntry {
        AllowEntry {
            case_id: case_id.to_string(),
            crate_name: crate_name.to_string(),
            reason: reason.to_string(),
        }
    }

    #[test]
    fn parses_entries_comments_and_blank_lines() {
        let source = "\
# a leading comment\n\
\n\
   \n\
wire/new_fun_ext_encode  beam_rt  an erl_eval lambda needs erl_eval\n\
proc_signals/monitor_port_unsupported\tbeam_bifs\tmonitor on a port\n\
  boot/proc_lib_start_link_blocks_the_boot_path   beam_engine   the call_ext boundary cannot suspend  \n";
        let list = Allowlist::parse(source, None).expect("parses");
        assert_eq!(list.len(), 3);
        assert_eq!(
            list.entries()[0],
            entry(
                "wire/new_fun_ext_encode",
                "beam_rt",
                "an erl_eval lambda needs erl_eval"
            )
        );
        // The reason is the remaining tokens joined by single spaces.
        assert_eq!(
            list.entries()[2],
            entry(
                "boot/proc_lib_start_link_blocks_the_boot_path",
                "beam_engine",
                "the call_ext boundary cannot suspend"
            )
        );
        assert!(!list.is_empty());
        assert!(list.contains("proc_signals/monitor_port_unsupported"));
        assert_eq!(
            list.get("proc_signals/monitor_port_unsupported").unwrap().reason,
            "monitor on a port"
        );
        assert!(list.get("nope").is_none());
        assert!(!list.contains("nope"));
    }

    #[test]
    fn strips_a_trailing_carriage_return() {
        let source = "a/b  crate  a reason\r\nc/d  crate2  another\r\n";
        let list = Allowlist::parse(source, None).expect("parses");
        assert_eq!(list.len(), 2);
        assert_eq!(list.entries()[0].case_id, "a/b");
        assert_eq!(list.entries()[0].reason, "a reason");
        assert_eq!(list.entries()[1].case_id, "c/d");
    }

    #[test]
    fn empty_source_is_an_empty_allowlist() {
        let list = Allowlist::parse("# only a comment\n\n", Some(PathBuf::from("x"))).expect("parses");
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert_eq!(list.path(), Some(Path::new("x")));
    }

    #[test]
    fn a_missing_crate_or_reason_is_a_usage_error_naming_the_line() {
        for (source, line) in [
            ("a/b  crate  reason\nonly-two-tokens  crate\n", 2),
            ("just-one-token\n", 1),
        ] {
            let err = Allowlist::parse(source, None).unwrap_err();
            assert!(matches!(err, Error::Usage(_)), "{err:?}");
            assert_eq!(err.exit_code(), 2);
            assert!(
                err.to_string().contains(&format!("line {line}")),
                "{err}"
            );
        }
    }

    #[test]
    fn a_duplicate_case_id_is_a_usage_error() {
        let source = "a/b  c1  first\na/b  c2  second\n";
        let err = Allowlist::parse(source, None).unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("duplicate"), "{err}");
        assert!(err.to_string().contains("a/b"), "{err}");
    }

    #[test]
    fn loading_a_missing_file_is_an_io_error() {
        let missing = std::env::temp_dir().join("differential-allowlist-does-not-exist-xyz.txt");
        let err = Allowlist::load(&missing).unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("reading the allowlist"), "{err}");
        assert!(err.to_string().contains("does-not-exist-xyz.txt"), "{err}");
    }

    #[test]
    fn loading_round_trips_the_path_and_entries() {
        let dir = std::env::temp_dir().join(format!(
            "differential-allowlist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("known-gaps.txt");
        std::fs::write(&path, "# header\na/b  some_crate  because\n").expect("write");
        let list = Allowlist::load(&path).expect("loads");
        assert_eq!(list.path(), Some(path.as_path()));
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries()[0], entry("a/b", "some_crate", "because"));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn empty_has_no_path_and_no_entries() {
        let list = Allowlist::empty();
        assert!(list.is_empty());
        assert_eq!(list.path(), None);
        assert!(list.entries().is_empty());
    }
}
