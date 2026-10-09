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

//! The host-identity helpers behind the `system` family's [`os_type`] and
//! [`system_architecture`] keys.
//!
//! Both keys describe the **host this build runs on**, so they are derived from
//! `std::env::consts` (the target `std` was compiled for) rather than from a
//! hard-coded literal — the previous `os_type` answered `{unix,linux}` on *every*
//! non-Windows target, which is wrong on macOS/BSD/… (the oracle there answers
//! `{unix,darwin}`, `{unix,freebsd}`, …).
//!
//! The split into a pure `*_for` function plus a thin accessor over
//! `std::env::consts` keeps every interesting mapping unit-testable without
//! cross-compiling.
//!
//! # The ERTS ground truth
//!
//! The legacy C builds the pair from a build-time string and a live `uname(2)`:
//!
//! * the family is the `os_type` string that the per-OS `sys.c` defines —
//!   `"unix"` on Unix (`erts/emulator/sys/unix/sys.c:835`) and `"win32"` on
//!   Windows (`erts/emulator/sys/win32/sys.c:395`);
//! * the name is the **lowercased `uname().sysname`** the C copies in
//!   `os_flavor/2` (`erts/emulator/sys/unix/sys.c:855-866`), which is
//!   `linux`/`darwin`/`freebsd`/… — i.e. the host's own OS name.
//!
//! [`os_type_for`] reproduces exactly that: the family of the OS's platform and
//! the OS name itself, lowercased. (On Windows the C's flavored name is `nt`; see
//! `os_flavor/2` in `sys/win32/sys.c`.)

use super::InfoValue;

/// The ERTS `{OsFamily,OsName}` pair for the OS named `os`.
///
/// `os` is a [`std::env::consts::OS`] name (`"linux"`, `"macos"`, `"windows"`,
/// `"freebsd"`, …); [`os_type`] passes the host's. Windows keeps its own
/// `{win32,nt}` family/name; every other mapped target is `{unix, <erts name>}`
/// with the ERTS names `linux`, `darwin`, `freebsd`, `openbsd`, `netbsd` and
/// `dragonfly` (`std::env::consts::OS` spells BSDs and DragonFly with the same
/// words the C's lowercased `sysname` yields, but macOS as `macos`, whose ERTS
/// name is `darwin`).
///
/// Any other target falls back to `{unix, os}` — the OS's own name, which is
/// exactly the C's rule (`os_flavor/2` copies the host's `sysname`), and the
/// Unix family the legacy C assumes for every non-Windows build. It is a
/// best-effort answer for a target this runtime has never been built for, not a
/// claim about it: a name no atom table holds cannot be materialized
/// ([`InfoValue::to_eterm`] reports [`BeamError::TermNotFound`] for it).
///
/// [`BeamError::TermNotFound`]: beam_common::BeamError::TermNotFound
pub(crate) fn os_type_for(os: &str) -> (&'static str, &str) {
    match os {
        "windows" => ("win32", "nt"),
        "linux" => ("unix", "linux"),
        "macos" => ("unix", "darwin"),
        "freebsd" => ("unix", "freebsd"),
        "openbsd" => ("unix", "openbsd"),
        "netbsd" => ("unix", "netbsd"),
        "dragonfly" => ("unix", "dragonfly"),
        other => ("unix", other),
    }
}

/// The Erlang `{OsFamily,OsName}` pair of the host.
///
/// Derived from [`std::env::consts::OS`] through [`os_type_for`], so a Linux
/// host answers `{unix,linux}` — the oracle's value, pinned key-for-key by
/// `tests/system_info_oracle.rs` (`os_type` is in `ORACLE_EQUAL_KEYS`) — while
/// macOS answers `{unix,darwin}`, FreeBSD `{unix,freebsd}`, Windows
/// `{win32,nt}`.
pub(crate) fn os_type() -> InfoValue {
    let (family, name) = os_type_for(std::env::consts::OS);
    InfoValue::Tuple(family, name)
}

/// The best-effort target triple for the architecture `arch` and OS `os`.
///
/// `std` exposes the architecture and the operating system but **not** the
/// vendor (the GNU/HW/pc field of a Rust target triple), so the vendor is never
/// invented: the known targets are spelled out, and everything else falls back
/// to `"<arch>-unknown-<os>"`. That fallback is exactly the canonical triple's
/// shape for the BSDs (`x86_64-unknown-freebsd`, …) and the closest honest
/// approximation elsewhere; it is introspection only, and the build itself
/// never depends on it.
pub(crate) fn architecture_for(arch: &str, os: &str) -> String {
    match (arch, os) {
        ("x86_64", "linux") => "x86_64-pc-linux-gnu".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        _ => format!("{arch}-unknown-{os}"),
    }
}

/// The target triple of this build, best-effort (see [`architecture_for`]).
pub(crate) fn system_architecture() -> String {
    architecture_for(std::env::consts::ARCH, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::AtomTable;

    use super::super::InfoValue;

    #[test]
    fn os_type_for_maps_every_known_platform() {
        assert_eq!(os_type_for("linux"), ("unix", "linux"));
        assert_eq!(os_type_for("macos"), ("unix", "darwin"));
        assert_eq!(os_type_for("windows"), ("win32", "nt"));
        assert_eq!(os_type_for("freebsd"), ("unix", "freebsd"));
        assert_eq!(os_type_for("openbsd"), ("unix", "openbsd"));
        assert_eq!(os_type_for("netbsd"), ("unix", "netbsd"));
        assert_eq!(os_type_for("dragonfly"), ("unix", "dragonfly"));
    }

    #[test]
    fn os_type_for_falls_back_to_the_os_name() {
        // Any other target is `{unix, <the OS's own name>}` — the legacy C's rule
        // (`os_flavor/2` copies the host's lowercased `sysname`).
        assert_eq!(os_type_for("solaris"), ("unix", "solaris"));
        assert_eq!(os_type_for("aix"), ("unix", "aix"));
    }

    #[test]
    fn os_type_derives_the_host_platform() {
        let (family, name) = os_type_for(std::env::consts::OS);
        assert_eq!(os_type(), InfoValue::Tuple(family, name));
        // On this host (Linux) that is the oracle's exact value:
        // `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(os_type)]).'`
        // prints `{unix,linux}`.
        if cfg!(target_os = "linux") {
            assert_eq!(os_type(), InfoValue::Tuple("unix", "linux"));
        }
        if cfg!(target_os = "macos") {
            assert_eq!(os_type(), InfoValue::Tuple("unix", "darwin"));
        }
        if cfg!(target_os = "windows") {
            assert_eq!(os_type(), InfoValue::Tuple("win32", "nt"));
        }
    }

    #[test]
    fn the_mapped_os_names_are_in_the_info_atom_vocabulary() {
        // `INFO_ATOMS` is the complete atom vocabulary of the surface; every name
        // `os_type_for` can answer for a *supported* platform must be in it, so
        // `system_info(os_type)` materializes on that platform.
        for name in [
            "unix",
            "linux",
            "darwin",
            "freebsd",
            "openbsd",
            "netbsd",
            "dragonfly",
            "win32",
            "nt",
        ] {
            assert!(
                super::super::INFO_ATOMS.contains(&name),
                "{name} must be seeded by INFO_ATOMS"
            );
        }
        // And the host's own answer really materializes.
        let mut heap = Heap::new(256).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(super::super::INFO_ATOMS.iter().copied());
        os_type()
            .to_eterm(&mut heap, &atoms)
            .expect("system_info(os_type) must materialize");
    }

    #[test]
    fn architecture_for_spells_the_known_targets_and_falls_back() {
        assert_eq!(architecture_for("x86_64", "linux"), "x86_64-pc-linux-gnu");
        assert_eq!(
            architecture_for("aarch64", "linux"),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(architecture_for("x86_64", "macos"), "x86_64-apple-darwin");
        assert_eq!(architecture_for("aarch64", "macos"), "aarch64-apple-darwin");
        // The fallback is the canonical vendor-less triple shape, which is right
        // for the BSDs and honest elsewhere.
        assert_eq!(
            architecture_for("x86_64", "freebsd"),
            "x86_64-unknown-freebsd"
        );
        assert_eq!(
            architecture_for("riscv64", "netbsd"),
            "riscv64-unknown-netbsd"
        );
    }

    #[test]
    fn system_architecture_names_this_arch_and_os() {
        let triple = system_architecture();
        assert!(!triple.is_empty());
        assert!(
            triple.contains(std::env::consts::ARCH),
            "{triple} must name the host architecture {}",
            std::env::consts::ARCH
        );
        assert_eq!(
            triple,
            architecture_for(std::env::consts::ARCH, std::env::consts::OS)
        );
    }
}
