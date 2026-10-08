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

//! OTP / ERTS version constants, parsing and normalized `Display`.
//!
//! The version strings are the ones a drop-in ERTS replacement must report to
//! `erlang:system_info(otp_release)`, `init:script_id/0`, `erlang:system_info(version)`
//! and the `erts-<vsn>` directory layout (compatibility contract A.6/A.7).
//!
//! Parsing is deliberately tolerant about *omitted* components (`MAJOR[.MINOR[.PATCH]]`,
//! missing components default to `0`) and strict about everything else: a non-numeric
//! component, a second `-`, an empty prerelease or trailing junk are rejected with a
//! [`ParseVersionError`]. Rendering is *normalized*: [`Version`] always displays the full
//! `MAJOR.MINOR.PATCH` triple, so the OTP release string `"30.0-rc0"` renders as
//! `"30.0.0-rc0"`.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// The OTP release this workspace targets (`erts_rust/../OTP_VERSION`).
pub const OTP_VERSION: &str = "30.0-rc0";

/// The ERTS version of that release (as reported by `erlang:system_info(version)`).
pub const ERTS_VERSION: &str = "17.1";

/// The ERTS version string in directory form (`erts-<vsn>`, as in `$ERL_TOP/bin`).
pub const ERTS_VSN: &str = "erts-17.1";

/// Parses the [`OTP_VERSION`] constant.
///
/// The constant is a compile-time literal known to be valid, so this cannot fail in
/// practice; a malformed constant would be a build-time bug and panics with a clear
/// message rather than silently returning a wrong version.
pub fn otp_version() -> Version {
    Version::parse(OTP_VERSION).expect("OTP_VERSION constant is a well-formed version")
}

/// Parses the [`ERTS_VERSION`] constant.
///
/// The constant is a compile-time literal known to be valid, so this cannot fail in
/// practice; a malformed constant would be a build-time bug and panics with a clear
/// message rather than silently returning a wrong version.
pub fn erts_version() -> Version {
    Version::parse(ERTS_VERSION).expect("ERTS_VERSION constant is a well-formed version")
}

/// A semantic OTP/ERTS version with an optional prerelease suffix.
///
/// Construct one with [`Version::new`], [`Version::parse`] or `str::parse`
/// ([`FromStr`]). Display is normalized to `MAJOR.MINOR.PATCH[-PRERELEASE]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The major version (`30` for OTP 30).
    pub major: u32,
    /// The minor version (`0` when omitted in the input).
    pub minor: u32,
    /// The patch version (`0` when omitted in the input).
    pub patch: u32,
    /// The prerelease tag (`rc0`, `alpha`, …) when the version is a prerelease.
    pub prerelease: Option<Prerelease>,
}

impl Version {
    /// Creates a release version (no prerelease) from its three numeric components.
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: None,
        }
    }

    /// Parses `MAJOR[.MINOR[.PATCH]][-PRERELEASE]`.
    ///
    /// Missing minor/patch components default to `0`. The input is rejected with a
    /// [`ParseVersionError`] when it is empty, when the major (or any present)
    /// component is not an ASCII number or overflows `u32`, when there are more than
    /// three dotted components, when a component is empty (`"30..1"`), or when the
    /// prerelease part is empty or invalid (see [`Prerelease::parse`]). A second `-`
    /// is rejected as well, so the prerelease label itself may not contain one.
    pub fn parse(s: &str) -> Result<Version, ParseVersionError> {
        if s.is_empty() {
            return Err(ParseVersionError::new(s, "empty version string"));
        }
        if s.matches('-').count() > 1 {
            return Err(ParseVersionError::new(s, "more than one '-' separator"));
        }

        let (core, prerelease) = match s.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (s, None),
        };

        let mut components = core.split('.');
        let major = match components.next() {
            Some(major) => parse_component(major, s, "major version is not a number")?,
            None => return Err(ParseVersionError::new(s, "missing major version")),
        };
        let minor = match components.next() {
            Some(minor) => parse_component(minor, s, "minor version is not a number")?,
            None => 0,
        };
        let patch = match components.next() {
            Some(patch) => parse_component(patch, s, "patch version is not a number")?,
            None => 0,
        };
        if components.next().is_some() {
            return Err(ParseVersionError::new(
                s,
                "more than three dotted components",
            ));
        }

        let prerelease = match prerelease {
            Some(prerelease) => Some(
                Prerelease::parse(prerelease)
                    .map_err(|error| ParseVersionError::new(s, error.reason()))?,
            ),
            None => None,
        };

        Ok(Version {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    /// Returns `true` when the version carries a prerelease tag.
    pub fn is_prerelease(&self) -> bool {
        self.prerelease.is_some()
    }
}

impl fmt::Display for Version {
    /// Renders the normalized `MAJOR.MINOR.PATCH` form plus `-{prerelease}` when
    /// present (so `"30.0"` renders as `"30.0.0"`, `"30.0-rc0"` as `"30.0.0-rc0"`, and a
    /// numberless prerelease label such as `"alpha"` as `"alpha0"`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(prerelease) = &self.prerelease {
            write!(f, "-{prerelease}")?;
        }
        Ok(())
    }
}

impl FromStr for Version {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Version::parse(s)
    }
}

/// The prerelease tag of a [`Version`]: a leading alphabetic label plus an optional
/// trailing number (`"rc0"` → label `"rc"`, number `0`; `"alpha"` → label `"alpha"`,
/// number `0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prerelease {
    /// The alphabetic label (`"rc"`, `"alpha"`, …).
    pub label: String,
    /// The trailing number, or `0` when the tag has none.
    pub number: u32,
}

impl Prerelease {
    /// Parses a prerelease tag: a non-empty alphabetic label optionally followed by
    /// ASCII digits.
    ///
    /// Returns a [`ParseVersionError`] (whose input is just the tag) when the label is
    /// empty or contains a non-alphabetic character, or when the trailing number
    /// overflows `u32`.
    pub fn parse(s: &str) -> Result<Self, ParseVersionError> {
        let label_end = s
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(s.len());
        let (label, digits) = s.split_at(label_end);

        if label.is_empty() {
            return Err(ParseVersionError::new(
                s,
                "prerelease label is empty or does not start with a letter",
            ));
        }
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseVersionError::new(
                s,
                "prerelease label must be followed by digits only",
            ));
        }

        let number = if digits.is_empty() {
            0
        } else {
            digits
                .parse::<u32>()
                .map_err(|_| ParseVersionError::new(s, "prerelease number overflows u32"))?
        };

        Ok(Prerelease {
            label: label.to_string(),
            number,
        })
    }
}

impl fmt::Display for Prerelease {
    /// Renders the tag as `{label}{number}` (`"rc0"`, `"alpha0"`, …).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.label, self.number)
    }
}

/// The error returned when a version string (or prerelease tag) cannot be parsed.
///
/// Carries the offending input and a short reason, both available through
/// [`input`](ParseVersionError::input) / [`reason`](ParseVersionError::reason) and
/// through `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseVersionError {
    input: String,
    reason: &'static str,
}

impl ParseVersionError {
    fn new(input: &str, reason: &'static str) -> Self {
        Self {
            input: input.to_string(),
            reason,
        }
    }

    /// The string that failed to parse.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// A short human-readable reason for the failure.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for ParseVersionError {
    /// Renders `invalid version input "…": reason`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid version input {:?}: {}", self.input, self.reason)
    }
}

impl Error for ParseVersionError {}

/// Parses one dotted component of a version, rejecting empty and non-ASCII-digit text.
fn parse_component(
    component: &str,
    input: &str,
    reason: &'static str,
) -> Result<u32, ParseVersionError> {
    if component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseVersionError::new(input, reason));
    }
    component
        .parse::<u32>()
        .map_err(|_| ParseVersionError::new(input, "numeric component overflows u32"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_the_targeted_release() {
        assert_eq!(OTP_VERSION, "30.0-rc0");
        assert_eq!(ERTS_VERSION, "17.1");
        assert_eq!(ERTS_VSN, "erts-17.1");
    }

    #[test]
    fn parse_otp_version() {
        let version = Version::parse(OTP_VERSION).expect("OTP_VERSION parses");
        assert_eq!(version.major, 30);
        assert_eq!(version.minor, 0);
        assert_eq!(version.patch, 0);
        assert_eq!(
            version.prerelease,
            Some(Prerelease {
                label: String::from("rc"),
                number: 0,
            })
        );
        assert!(version.is_prerelease());
        assert_eq!(version.to_string(), "30.0.0-rc0");
    }

    #[test]
    fn parse_erts_version() {
        let version = Version::parse(ERTS_VERSION).expect("ERTS_VERSION parses");
        assert_eq!(version.major, 17);
        assert_eq!(version.minor, 1);
        assert_eq!(version.patch, 0);
        assert_eq!(version.prerelease, None);
        assert!(!version.is_prerelease());
        assert_eq!(version.to_string(), "17.1.0");
    }

    #[test]
    fn version_helpers_parse_the_constants() {
        assert_eq!(otp_version(), Version::parse(OTP_VERSION).unwrap());
        assert_eq!(otp_version().major, 30);
        assert_eq!(erts_version(), Version::parse(ERTS_VERSION).unwrap());
        assert_eq!(erts_version().minor, 1);
    }

    #[test]
    fn new_builds_a_release_version() {
        let version = Version::new(1, 2, 3);
        assert_eq!(version.prerelease, None);
        assert!(!version.is_prerelease());
        assert_eq!(version.to_string(), "1.2.3");
    }

    #[test]
    fn display_normalizes_and_round_trips() {
        assert_eq!(Version::parse("30.0").unwrap().to_string(), "30.0.0");
        assert_eq!(Version::parse("30").unwrap().to_string(), "30.0.0");
        // A prerelease label without a number renders the canonical `{label}0`.
        assert_eq!(
            Version::parse("0.0.1-alpha").unwrap().to_string(),
            "0.0.1-alpha0"
        );
        for input in [
            "30.0.0-rc0",
            "17.1.0",
            "1.2.3",
            "0.0.1-alpha0",
            "12.0.4-beta2",
        ] {
            let version = Version::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(version.to_string(), input);
            assert_eq!(Version::parse(&version.to_string()).unwrap(), version);
            assert_eq!(Version::parse(input).unwrap(), version);
        }
    }

    #[test]
    fn equivalent_spellings_parse_to_the_same_version() {
        assert_eq!(
            Version::parse("0.0.1-alpha").unwrap(),
            Version::parse("0.0.1-alpha0").unwrap()
        );
        assert_eq!(
            Version::parse("30").unwrap(),
            Version::parse("30.0.0").unwrap()
        );
    }
    #[test]
    fn from_str_parses() {
        let version: Version = "30.0-rc0".parse().expect("parses via FromStr");
        assert_eq!(version.to_string(), "30.0.0-rc0");
        assert!("".parse::<Version>().is_err());
    }

    #[test]
    fn malformed_inputs_are_rejected() {
        for input in [
            "",
            "abc",
            "30.x",
            "x.1",
            "30..1",
            "30.1.",
            "-rc0",
            "30.0-",
            "30.0-rc-1",
            "30.0.0.1",
            "30.1.2.3",
            "1e3",
            "+30",
            "30 ",
            " 30",
        ] {
            assert!(
                Version::parse(input).is_err(),
                "expected {input:?} to be rejected"
            );
        }
    }

    #[test]
    fn parse_error_reports_input_and_reason() {
        let error = Version::parse("30.x").expect_err("must fail");
        assert_eq!(error.input(), "30.x");
        assert_eq!(error.reason(), "minor version is not a number");
        assert_eq!(
            error.to_string(),
            "invalid version input \"30.x\": minor version is not a number"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn prerelease_parse_and_display() {
        assert_eq!(
            Prerelease::parse("rc0").unwrap(),
            Prerelease {
                label: String::from("rc"),
                number: 0,
            }
        );
        assert_eq!(
            Prerelease::parse("alpha").unwrap(),
            Prerelease {
                label: String::from("alpha"),
                number: 0,
            }
        );
        assert_eq!(
            Prerelease::parse("beta12").unwrap(),
            Prerelease {
                label: String::from("beta"),
                number: 12,
            }
        );
        assert_eq!(Prerelease::parse("rc0").unwrap().to_string(), "rc0");
        assert_eq!(Prerelease::parse("alpha").unwrap().to_string(), "alpha0");
    }

    #[test]
    fn prerelease_rejects_malformed_tags() {
        for input in ["", "0", "0rc", "rc-1", "rc0x", "rc."] {
            assert!(
                Prerelease::parse(input).is_err(),
                "expected {input:?} to be rejected"
            );
        }
    }
}
