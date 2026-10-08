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

//! The `ERL_*FLAGS` environment-variable surface: the tokenizer and the argv merge.
//!
//! Real `erl` merges four environment variables into the command line **before** `init`
//! ever sees it (`erts/etc/common/erlexec.c`, `initial_argv_massage`; erlexec.c:2179-2203):
//!
//! ```text
//! [ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]
//! ```
//!
//! so `ERL_AFLAGS` is **prepended** before the command line and `ERL_FLAGS` then
//! `ERL_ZFLAGS` are **appended** after it. Every merged token is handed to the same
//! parser as a real switch, so a tokenized environment flag is indistinguishable from a
//! command-line flag to `init:get_argument/1` (verified against OTP 29.1.1 / erts-17.1):
//!
//! ```text
//! ERL_FLAGS='-setcookie envcookie' erl -noshell -eval '…init:get_argument(setcookie)…'
//!   → {ok,[["envcookie"]]}
//! ```
//!
//! `<sysver>` is the OTP **system version** — the major of `OTP_VERSION`
//! (`OTP_SYSTEM_VERSION` in `erts/etc/common/Makefile.in`; `SYSTEM_VSN` from
//! `OTP_VERSION | sed 's|\([0-9]*\).*|\1|'`), so this runtime's variable is
//! [`otp_flags_var`] = `ERL_OTP30_FLAGS`. The variable is intentionally undocumented in
//! OTP and meant for internal use only.
//!
//! # The tokenizer
//!
//! [`tokenize`] reproduces `build_args_from_string(string, allow_comments = 0)` exactly
//! (erlexec.c:1772-1930), which is what `build_args_from_env` calls for every variable.
//! The full rule set is documented on the function.

use std::collections::HashMap;

/// The process/instance environment the merge reads variables from.
///
/// It is a trait so the merge is testable without touching the real process environment
/// (see [`MapEnv`]); [`ProcessEnv`] is the production implementation.
pub trait Env {
    /// The value of the environment variable `name`, or `None` when it is unset (or not
    /// valid UTF-8, which [`ProcessEnv`] folds into "unset").
    fn get(&self, name: &str) -> Option<String>;
}

/// The real process environment, read through [`std::env::var`].
///
/// A variable that is set but **not valid UTF-8** yields `None` — it is treated as unset,
/// exactly as the C's byte-oriented `getenv` would be unusable here (this runtime's
/// switch grammar is UTF-8 `String`s).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed, in-memory environment — the unit-testable [`Env`].
///
/// ```
/// use beam_rt::cli::{Env, MapEnv};
/// use std::collections::HashMap;
///
/// let env = MapEnv(HashMap::from([("ERL_FLAGS".to_string(), "-noshell".to_string())]));
/// assert_eq!(env.get("ERL_FLAGS").as_deref(), Some("-noshell"));
/// assert_eq!(env.get("ERL_AFLAGS"), None);
/// ```
#[derive(Debug, Clone, Default)]
pub struct MapEnv(pub HashMap<String, String>);

impl Env for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// The name of the OTP-internal `ERL_OTP<sysver>_FLAGS` variable.
///
/// `<sysver>` is the OTP **system version** — the major of
/// [`beam_common::OTP_VERSION`] (`OTP_SYSTEM_VERSION` /
/// `SYSTEM_VSN`, `erts/etc/common/Makefile.in`), so this runtime targets
/// `ERL_OTP30_FLAGS`.
pub fn otp_flags_var() -> String {
    format!("ERL_OTP{}_FLAGS", beam_common::otp_version().major)
}

/// Whether `c` is one of the six ASCII whitespace bytes the C splits tokens on
/// (`Build0`/`Build`, erlexec.c:1800-1811): space, tab, newline, carriage return, form
/// feed (`\x0c`) and vertical tab (`\x0b`). This is **not** Unicode whitespace.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Splits an environment-variable string into argv tokens the way real `erl` does
/// (`build_args_from_string` with `allow_comments = 0`; erlexec.c:1772-1930).
///
/// The rules, in full:
///
/// * tokens are separated by runs of the six ASCII whitespace bytes — space, `\t`, `\n`,
///   `\r`, `\x0b` (vertical tab) and `\x0c` (form feed);
/// * a `'…'` or `"…"` group is **one** token with the quotes removed; the matching quote
///   also ends the group, and quotes may appear anywhere inside a token, so
///   `"a b"'c d'` is the single token `a bc d` and `ab"c d"ef` is `abc def`;
/// * `\` escapes (literalizes) the **next** character, including whitespace and a quote,
///   so `a\ b` is the single token `a b`;
/// * `#` is **not** a comment — it is an ordinary character (verified:
///   `ERL_FLAGS='-e a#b'` gives `init:get_argument(e)` = `{ok,[["a#b"]]}`);
/// * an empty string yields no tokens (`[]`); a quoted empty token (`""`) yields one
///   empty token.
///
/// ```
/// use beam_rt::cli::tokenize;
///
/// assert_eq!(tokenize("  -a  1\t-b \"two words\"  "), ["-a", "1", "-b", "two words"]);
/// assert_eq!(tokenize("one\\ two"), ["one two"]);
/// assert_eq!(tokenize("a#b"), ["a#b"]);
/// assert_eq!(tokenize(""), Vec::<String>::new());
/// ```
pub fn tokenize(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        // Skip the separator run (the C `Build0` state) — an all-whitespace suffix ends
        // the scan without producing a token.
        while index < chars.len() && is_whitespace(chars[index]) {
            index += 1;
        }
        if index >= chars.len() {
            break;
        }
        let mut token = String::new();
        // The C `Build` state: accumulate until an unescaped/unquoted separator.
        loop {
            let Some(&c) = chars.get(index) else {
                break;
            };
            match c {
                '"' | '\'' => {
                    // A quote opens a group; the *same* quote closes it (end-of-input
                    // also closes it, exactly as the C's `'\0'` transition does).
                    index += 1;
                    while let Some(&q) = chars.get(index) {
                        index += 1;
                        if q == c {
                            break;
                        }
                        token.push(q);
                    }
                }
                '\\' => {
                    // Escape the next character literally (`AcceptNext`).
                    index += 1;
                    if let Some(&next) = chars.get(index) {
                        token.push(next);
                        index += 1;
                    }
                }
                c if is_whitespace(c) => break,
                c => {
                    token.push(c);
                    index += 1;
                }
            }
        }
        tokens.push(token);
    }
    tokens
}

/// Merges the `ERL_*FLAGS` environment variables into `args` in real `erl`'s order.
///
/// The result reproduces `initial_argv_massage`'s ordering exactly (erlexec.c:2179-2203):
///
/// ```text
/// [ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] args… [ERL_FLAGS] [ERL_ZFLAGS]
/// ```
///
/// An **unset** variable contributes nothing; every set variable is [`tokenize`]d, so a
/// variable holding several switches expands to several tokens (and the same variable
/// holding only whitespace expands to none). `args` (the command line, excluding
/// `argv[0]`) keeps its order in the middle.
///
/// ```
/// use beam_rt::cli::{merge_argv, MapEnv};
/// use std::collections::HashMap;
///
/// let env = MapEnv(HashMap::from([
///     ("ERL_AFLAGS".to_string(), "-a".to_string()),
///     ("ERL_FLAGS".to_string(), "-f".to_string()),
/// ]));
/// let args = vec!["-cmd".to_string()];
/// assert_eq!(merge_argv(&args, &env), ["-a", "-cmd", "-f"]);
/// ```
pub fn merge_argv(args: &[String], env: &dyn Env) -> Vec<String> {
    let mut merged = Vec::new();
    // Prepended before the command line, in order.
    for name in [otp_flags_var().as_str(), "ERL_AFLAGS"] {
        if let Some(value) = env.get(name) {
            merged.extend(tokenize(&value));
        }
    }
    merged.extend(args.iter().cloned());
    // Appended after the command line, in order.
    for name in ["ERL_FLAGS", "ERL_ZFLAGS"] {
        if let Some(value) = env.get(name) {
            merged.extend(tokenize(&value));
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expected tokens as owned `String`s (the left side of every assertion).
    fn tokens(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn whitespace_runs_are_a_single_separator() {
        assert_eq!(tokenize("  a   b  "), tokens(&["a", "b"]));
        assert_eq!(
            tokenize("a\tb\nc\rd\x0be\x0cf"),
            tokens(&["a", "b", "c", "d", "e", "f"])
        );
    }

    #[test]
    fn quotes_group_and_are_stripped() {
        assert_eq!(tokenize("\"a b\""), tokens(&["a b"]));
        assert_eq!(tokenize("'a b'"), tokens(&["a b"]));
        // Quotes may open mid-token and concatenate with the surrounding text.
        assert_eq!(tokenize("ab\"c d\"ef"), tokens(&["abc def"]));
        assert_eq!(tokenize("\"abc\"def"), tokens(&["abcdef"]));
    }

    #[test]
    fn adjacent_quotes_concatenate_into_one_token() {
        // Verified against the oracle: `ERL_FLAGS="-g \"a b\"'c d'"` makes
        // `init:get_argument(g)` = `{ok,[["a bc d"]]}` — one token, quotes stripped.
        assert_eq!(tokenize("\"a b\"'c d'"), tokens(&["a bc d"]));
    }

    #[test]
    fn backslash_escapes_the_next_character() {
        assert_eq!(tokenize("one\\ two"), tokens(&["one two"]));
        // An escaped quote is literal, not a group delimiter.
        assert_eq!(tokenize("a\\\"b"), tokens(&["a\"b"]));
        // A trailing backslash escapes nothing and does not swallow the token.
        assert_eq!(tokenize("abc\\"), tokens(&["abc"]));
    }

    #[test]
    fn hash_is_an_ordinary_character_not_a_comment() {
        assert_eq!(tokenize("a#b"), tokens(&["a#b"]));
        assert_eq!(tokenize("#just a flag"), tokens(&["#just", "a", "flag"]));
    }

    #[test]
    fn empty_input_yields_no_tokens() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   \t\n").is_empty());
        // A quoted empty group, however, is one empty token.
        assert_eq!(tokenize("\"\""), tokens(&[""]));
    }

    #[test]
    fn a_multi_token_string_expands_to_many_args() {
        assert_eq!(
            tokenize("-setcookie cookie -pa /a -pz \"/x y\""),
            tokens(&["-setcookie", "cookie", "-pa", "/a", "-pz", "/x y"])
        );
    }

    #[test]
    fn merge_prepends_and_appends_in_the_oracle_order() {
        let env = MapEnv(HashMap::from([
            (otp_flags_var(), "-otp".to_string()),
            ("ERL_AFLAGS".to_string(), "-af".to_string()),
            ("ERL_FLAGS".to_string(), "-f".to_string()),
            ("ERL_ZFLAGS".to_string(), "-z".to_string()),
        ]));
        let args = tokens(&["-cmd", "1"]);
        assert_eq!(
            merge_argv(&args, &env),
            tokens(&["-otp", "-af", "-cmd", "1", "-f", "-z"])
        );
    }

    #[test]
    fn merge_omits_unset_variables() {
        // Nothing set ⇒ the command line is returned unchanged.
        let args = tokens(&["-noshell", "-eval", "1"]);
        assert_eq!(merge_argv(&args, &MapEnv::default()), args);

        // Only `ERL_FLAGS` set ⇒ appended after the command line.
        let env = MapEnv(HashMap::from([("ERL_FLAGS".to_string(), "-f".to_string())]));
        assert_eq!(
            merge_argv(&args, &env),
            tokens(&["-noshell", "-eval", "1", "-f"])
        );

        // Only `ERL_AFLAGS` set ⇒ prepended before the command line.
        let env = MapEnv(HashMap::from([(
            "ERL_AFLAGS".to_string(),
            "-af".to_string(),
        )]));
        assert_eq!(
            merge_argv(&args, &env),
            tokens(&["-af", "-noshell", "-eval", "1"])
        );
    }

    #[test]
    fn merge_tokenizes_each_variable_independently() {
        let env = MapEnv(HashMap::from([
            ("ERL_AFLAGS".to_string(), "-a 1".to_string()),
            (
                "ERL_FLAGS".to_string(),
                "-b 2 -c \"three four\"".to_string(),
            ),
        ]));
        let args = tokens(&["-cmd"]);
        assert_eq!(
            merge_argv(&args, &env),
            tokens(&["-a", "1", "-cmd", "-b", "2", "-c", "three four"])
        );
    }

    #[test]
    fn erl_flags_precedes_erl_zflags() {
        // The later occurrence (ERL_FLAGS) must appear before ERL_ZFLAGS and after the
        // command line, so a program observing both sees them in that order.
        let env = MapEnv(HashMap::from([
            ("ERL_FLAGS".to_string(), "-setcookie f".to_string()),
            ("ERL_ZFLAGS".to_string(), "-setcookie z".to_string()),
        ]));
        let args = tokens(&["-setcookie", "cmd"]);
        assert_eq!(
            merge_argv(&args, &env),
            tokens(&["-setcookie", "cmd", "-setcookie", "f", "-setcookie", "z"])
        );
    }

    #[test]
    fn an_absent_variable_is_read_as_none() {
        let env = MapEnv(HashMap::from([("ERL_FLAGS".to_string(), "x".to_string())]));
        assert_eq!(env.get("ERL_FLAGS").as_deref(), Some("x"));
        assert_eq!(env.get("ERL_AFLAGS"), None);
    }

    #[test]
    fn the_otp_flags_variable_is_named_for_the_system_version() {
        assert_eq!(beam_common::otp_version().major, 30);
        assert_eq!(otp_flags_var(), "ERL_OTP30_FLAGS");
    }
}
