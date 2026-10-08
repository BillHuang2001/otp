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

//! Shared helpers for this crate's unit tests (`#[cfg(test)]` only).
//!
//! Both [`crate::module_build`] and [`crate::nif_build`] drive a stub compiler from a scratch
//! directory, so the scratch directory and the `erlc`-invocation helpers live here rather than
//! being duplicated in each test module. This module is compiled only under `cfg(test)`.

use std::path::{Path, PathBuf};

/// An executable scratch directory (a `#!/bin/sh` stub lives in it), removed on drop.
pub(crate) struct Scratch {
    /// The directory; helpers write their scripts into it.
    pub(crate) dir: PathBuf,
}

impl Scratch {
    /// A fresh scratch directory for one test (`tag` keeps the name readable).
    pub(crate) fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "differential-module-build-{}-{}-{:?}",
            std::process::id(),
            tag,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }

    /// Write `body` to `dir/<name>`, make it executable on Unix, and return its path.
    pub(crate) fn executable(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// An `erlc` stub that writes the expected `.beam` and records every invocation (one line per
/// run) in `counter`.
pub(crate) fn counting_compiler(scratch: &Scratch, counter: &Path) -> PathBuf {
    let body = format!(
        "#!/bin/sh\necho x >> '{counter}'\nout=\nsrc=\n\
         while [ $# -gt 0 ]; do case $1 in -o) shift; out=$1;; *) src=$1;; esac; shift; done\n\
         base=$(basename \"$src\" .erl)\nprintf 'BEAM' > \"$out/$base.beam\"\n",
        counter = counter.display()
    );
    scratch.executable("erlc", &body)
}

/// How many times a stub compiler above has run.
pub(crate) fn invocations(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// A stub compiler that writes the expected `.beam`, so a fixture build can be driven without
/// a real `erlc`.
pub(crate) fn fabricating_compiler(scratch: &Scratch) -> PathBuf {
    scratch.executable(
        "erlc",
        "#!/bin/sh\nout=\nsrc=\nwhile [ $# -gt 0 ]; do case $1 in -o) shift; out=$1;; *) src=$1;; esac; shift; done\n\
         base=$(basename \"$src\" .erl)\nprintf 'BEAM' > \"$out/$base.beam\"\n",
    )
}
