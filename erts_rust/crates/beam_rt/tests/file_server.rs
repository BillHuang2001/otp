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

//! The runtime-native **`file_server_2` service** (contract A.9), end to end.
//!
//! `lib/kernel/src/file.erl` routes every non-`raw` file operation through
//! `gen_server:call(file_server_2, Request, infinity)` (`file.erl`'s `check_and_call/2` and
//! `call/2`), and `c:erlangrc/0` — the last directive of the real
//! `bootstrap/bin/start.boot` — is the first such caller. `beam_rt`'s
//! [`FileServices`](../../src/native/file_server.rs) supplies the missing service process,
//! so these tests drive the real `file:*` client code over **real temporary files and
//! directories** and cross-check every answer against the live `erl`.
//!
//! Two shapes are pinned:
//!
//! * **The errno mapping.** `file:open/2` on a missing path is `{error, enoent}` and on a
//!   directory is `{error, eisdir}` — the second needs the explicit `is_dir()` check, because
//!   Linux's `open(2)` *succeeds* on a directory (the oracle's `efile_open` answers `EISDIR`).
//! * **The payload.** `file:read_file/1` returns the file's bytes verbatim, and `file:list_dir/1`
//!   returns the entries in the host's directory order (`file.erl:1044` documents that the names
//!   are *not* sorted — the test sorts them itself so the comparison against the oracle is
//!   order-independent).
//!
//! The io-device's `{get_until, Enc, Prompt, Mod, Func, XtraArgs}` request IS served: the
//! device's read loop drives an engine-backed `beam_io::io_device::Continuation` that runs the
//! real `erl_scan:tokens/4` (`../../src/native/file_server/get_until.rs`), and the
//! library-level `io:scan_erl_exprs`/`io:parse_erl_exprs` path over it is driven end to end by
//! the sibling [`io_scan.rs`](io_scan.rs); the request itself is pinned in
//! `../../src/native/file_server/get_until/tests.rs`.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_engine::terms;
use beam_rt::Runtime;

mod support;

/// Renders `call` under a fresh runtime, then asserts the oracle renders the **same** value
/// (`io:format("~p", [Call])` under `erl -noshell`). The oracle's rendering is the expected
/// value, so a printer divergence shows up here as a mismatch instead of being hardcoded away.
///
/// A missing toolchain — or an oracle that cannot answer — is a loud skip: the runtime's own
/// rendering is still returned, so the caller's own assertions still run.
fn renders_like_the_oracle(call: &str) -> String {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let value = runtime.eval(call).expect(call);
    // `io:format` terminates the term with its formatter's newline; the oracle probe prints
    // the same term through the same formatter and is trimmed, so compare trimmed values.
    let rendered = runtime
        .format_term(value, None)
        .expect("a printable term")
        .trim_end()
        .to_string();
    let Some(oracle) = support::require_oracle("file_server") else {
        return rendered;
    };
    let probe = format!("io:format(\"~p\", [{call}])");
    let Some(expected) = support::oracle_eval(&oracle, None, &probe) else {
        eprintln!("warning: the oracle could not answer `{call}`");
        return rendered;
    };
    assert_eq!(rendered, expected, "the oracle renders `{call}` differently");
    rendered
}

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_file_server_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_file_server_{}_{unique}_{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }

    /// The directory itself.
    fn path(&self) -> &Path {
        &self.0
    }

    /// A path inside the directory.
    fn child(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The **Erlang string literal** for `path` — a path has no character Rust's `Debug` escapes
/// differently from Erlang's string syntax for the ASCII-only names these tests build.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// `file:open/2` on a path that does not exist is the `{error, enoent}` that unblocks
/// `c:erlangrc/0`'s `file:path_eval/2` (`lib/stdlib/src/c.erl:1069`).
#[test]
fn opening_a_missing_file_reports_enoent() {
    let dir = TempDir::new("missing");
    let call = format!("file:open({}, [read])", literal(&dir.child("absent.erl")));
    assert_eq!(
        renders_like_the_oracle(&call),
        "{error,enoent}",
        "a missing path is enoent (`lib/kernel/src/file.erl:2960`)"
    );
}

/// `file:open/2` on a **directory** is `{error, eisdir}`. Linux's `open(2)` succeeds on a
/// directory, so the service must consult `metadata().is_dir()` before opening.
#[test]
fn opening_a_directory_reports_eisdir() {
    let dir = TempDir::new("eisdir");
    let call = format!("file:open({}, [read])", literal(dir.path()));
    assert_eq!(
        renders_like_the_oracle(&call),
        "{error,eisdir}",
        "a directory asked for as a file is eisdir, not a live handle"
    );
}

/// `file:read_file/1` on an existing file of Erlang expressions returns its bytes verbatim:
/// the runtime's `{ok, Binary}` is cross-checked against the oracle *and* its payload is
/// compared with the bytes on disk.
#[test]
fn reading_an_existing_file_returns_its_bytes() {
    let dir = TempDir::new("read");
    let path = dir.child("sample.erl");
    let contents = b"-module(sample).\n";
    fs::write(&path, contents).expect("write the fixture file");

    let call = format!("file:read_file({})", literal(&path));
    let rendered = renders_like_the_oracle(&call);
    assert!(
        rendered.starts_with("{ok,<<"),
        "expected the oracle's `{{ok,Binary}}`, got {rendered}"
    );

    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let value = runtime.eval(&call).expect(&call);
    let payload = terms::tuple_element(value, 1).expect("the binary element");
    assert_eq!(
        terms::binary_bytes(payload).expect("a binary"),
        &contents[..],
        "the payload is the file's bytes"
    );
}

/// `file:list_dir/1` answers through the service too; the test sorts the entries itself
/// because the oracle's order is the host's directory order and is not specified.
#[test]
fn listing_a_directory_returns_every_entry() {
    let dir = TempDir::new("list");
    for name in ["a.erl", "b.erl", "c.erl"] {
        fs::write(dir.child(name), b"x").expect("write an entry");
    }
    let call = format!(
        "lists:sort(element(2, file:list_dir({})))",
        literal(dir.path())
    );
    assert_eq!(
        renders_like_the_oracle(&call),
        "[\"a.erl\",\"b.erl\",\"c.erl\"]",
        "every entry in the directory is listed"
    );
}
