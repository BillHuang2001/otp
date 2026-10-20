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
//!
//! # Plain arguments
//!
//! A merged token stream also carries the **plain arguments** `init:get_plain_arguments/0`
//! reports: `-extra` (everything after it, in that entity) and `--` (everything after it up
//! to the next switch-looking token). [`split_plain`] does that split **per entity**, and
//! [`merge_argv_with_plain`] is the merge that keeps the flags and the plain tails apart —
//! `init`'s own rule, where an `-extra` "only effects the remaining command-line flags in
//! the entity in which it is passed" and the tails concatenate in merge order
//! (`erts/preloaded/src/init.erl:115-139,1664-1740`).

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
/// This is the **flag stream only** — a thin delegation to
/// [`merge_argv_with_plain`], which also returns the plain-argument tail `-extra`/`--`
/// select. The two agree on every flag token.
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
    merge_argv_with_plain(args, env).0
}

/// Splits one token stream into its **flags** and its **plain arguments**.
///
/// The stream is one **entity** — the command line, or one `ERL_*FLAGS` variable — because
/// that is the unit `-extra` acts on: `init`'s own rule is that an `-extra` "only effects
/// the remaining command-line flags in the entity in which it is passed"
/// (`erts/preloaded/src/init.erl:127-139`). The rules, in full
/// (`parse_boot_args/4` + `check/2`, `init.erl:1664-1740`):
///
/// * in flag mode, a token `-extra` is **consumed** and **every** following token of the
///   stream is a plain argument — including a later literal `-extra`, which is then data
///   and never a switch. Oracle: `erl -noshell -extra a b -extra c` ⇒
///   `init:get_plain_arguments()` is `["a","b","-extra","c"]`, and an `-eval` placed after
///   the `-extra` is never evaluated (it becomes a plain argument).
/// * a token `--` is **consumed** and the tokens after it are plain **until the next token
///   that looks like a switch** — one beginning with `-` or `+` — which stays in the flag
///   stream and **resumes** flag mode. Oracle: `-- a b` ⇒ `["a","b"]`; `-- a -noshell b` ⇒
///   `["a"]`, with `-noshell` and the value it takes (`b`) in the flag stream; and
///   `-- a -extra b` ⇒ `["a","b"]` (the `-extra` is switch-looking, so it ends the `--` run
///   and then claims the rest of the stream).
/// * every **other** token, a bare positional one included, stays in the flag stream: this
///   splitter never guesses a switch's arity, so deciding that `b` in `-noshell b` is
///   `-noshell`'s value — and not a plain argument — is the parser's business, not this
///   splitter's (`get_args/2`, `init.erl:1724-1737`).
/// * a stream with **no** marker is entirely flags, in order, so an entity without `-extra`
///   or `--` never changes anything.
/// * a **bare** `-extra` (nothing after it) contributes no plain argument at all.
///
/// The two halves are returned as `(flags, plain)`; both keep their stream order.
pub fn split_plain(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut flags = Vec::new();
    let mut plain = Vec::new();
    let mut index = 0usize;
    while let Some(token) = args.get(index) {
        if token == "-extra" {
            // `-extra` claims the whole rest of this entity, itself excluded.
            plain.extend(args[index + 1..].iter().cloned());
            break;
        }
        if token == "--" {
            // `--` opens a plain run that the next switch-looking token ends.
            index += 1;
            while let Some(candidate) = args.get(index) {
                if looks_like_a_switch(candidate) {
                    break;
                }
                plain.push(candidate.clone());
                index += 1;
            }
            continue;
        }
        flags.push(token.clone());
        index += 1;
    }
    (flags, plain)
}

/// Whether `token` looks like an emulator switch — it begins with `-` or `+`.
///
/// That is exactly the test that ends a `--` plain run (`check/2`,
/// `erts/preloaded/src/init.erl:1720-1722`: anything matching `<<"-",Flag/binary>>` is a
/// flag, everything else is an argument; the `+` switches are the emulator's own `+A`/`+S`
/// family, which this runtime's parser also accepts).
fn looks_like_a_switch(token: &str) -> bool {
    token.starts_with('-') || token.starts_with('+')
}

/// Merges the `ERL_*FLAGS` environment variables into `args`, keeping the **plain
/// arguments** apart from the flags.
///
/// The five entities are split **independently** and concatenated in the merge order of
/// [`merge_argv`] (`initial_argv_massage`, erlexec.c:2179-2203):
///
/// ```text
/// [ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] args… [ERL_FLAGS] [ERL_ZFLAGS]
/// ```
///
/// Each entity is [`tokenize`]d and then [`split_plain`]d, so its flags join the flag
/// stream **in merge order** and its plain tail joins the plain list **in the same order** —
/// `init`'s rule that "if multiple `-extra` flags are passed they are concatenated using the
/// same order rules as `ERL_*FLAGS`" (`erts/preloaded/src/init.erl:127-139`). An entity with
/// no marker is unaffected and an **unset** variable contributes nothing.
///
/// The documented example reproduces exactly (`init.erl:135-139`):
///
/// ```text
/// ERL_AFLAGS="-extra a" ERL_ZFLAGS="-extra d" erl -extra b -extra c
///   init:get_plain_arguments() → ["a","b","-extra","c","d"]
/// ```
///
/// — each entity splits at its **own** first `-extra`, and the command line's tail carries
/// its later literal `-extra` as data.
pub fn merge_argv_with_plain(args: &[String], env: &dyn Env) -> (Vec<String>, Vec<String>) {
    let mut flags = Vec::new();
    let mut plain = Vec::new();
    // One entity at a time, in merge order: flags in merge order, plain tails likewise.
    let mut absorb = |tokens: Vec<String>| {
        let (entity_flags, entity_plain) = split_plain(&tokens);
        flags.extend(entity_flags);
        plain.extend(entity_plain);
    };
    // Prepended before the command line, in order.
    for name in [otp_flags_var().as_str(), "ERL_AFLAGS"] {
        if let Some(value) = env.get(name) {
            absorb(tokenize(&value));
        }
    }
    absorb(args.to_vec());
    // Appended after the command line, in order.
    for name in ["ERL_FLAGS", "ERL_ZFLAGS"] {
        if let Some(value) = env.get(name) {
            absorb(tokenize(&value));
        }
    }
    (flags, plain)
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

    // -- plain arguments: `-extra` and `--` ----------------------------------

    #[test]
    fn a_stream_with_no_marker_is_entirely_flags() {
        // The common case: nothing plain, every token a flag-stream token in order.
        let (flags, plain) = split_plain(&tokens(&["-noshell", "-eval", "1"]));
        assert_eq!(flags, tokens(&["-noshell", "-eval", "1"]));
        assert!(plain.is_empty());
    }

    #[test]
    fn extra_claims_the_whole_rest_of_the_stream() {
        // Oracle: `erl -noshell -eval … -extra a b c` ⇒ `["a","b","c"]`.
        let (flags, plain) = split_plain(&tokens(&["-noshell", "-extra", "a", "b", "c"]));
        assert_eq!(flags, tokens(&["-noshell"]));
        assert_eq!(plain, tokens(&["a", "b", "c"]));
    }

    #[test]
    fn a_second_extra_is_data_once_the_stream_is_plain() {
        // Oracle: `-extra a b -extra c` ⇒ `["a","b","-extra","c"]` — a later literal
        // `-extra` inside an already-plain tail is carried as a plain argument.
        let (flags, plain) = split_plain(&tokens(&["-noshell", "-extra", "a", "b", "-extra", "c"]));
        assert_eq!(flags, tokens(&["-noshell"]));
        assert_eq!(plain, tokens(&["a", "b", "-extra", "c"]));
    }

    #[test]
    fn a_bare_extra_contributes_no_plain_argument() {
        let (flags, plain) = split_plain(&tokens(&["-noshell", "-extra"]));
        assert_eq!(flags, tokens(&["-noshell"]));
        assert!(plain.is_empty());
    }

    #[test]
    fn the_end_args_marker_opens_a_plain_run() {
        // Oracle: `-- a b` ⇒ `["a","b"]`.
        let (flags, plain) = split_plain(&tokens(&["--", "a", "b"]));
        assert!(flags.is_empty());
        assert_eq!(plain, tokens(&["a", "b"]));
    }

    #[test]
    fn a_switch_ends_the_end_args_run_and_resumes_flag_mode() {
        // Oracle: `-- a -noshell b` ⇒ `["a"]`, with `-noshell` — and the value it takes,
        // `b` — in the flag stream (this splitter never guesses a switch's arity).
        let (flags, plain) = split_plain(&tokens(&["--", "a", "-noshell", "b"]));
        assert_eq!(flags, tokens(&["-noshell", "b"]));
        assert_eq!(plain, tokens(&["a"]));
    }

    #[test]
    fn an_extra_after_the_end_args_run_claims_the_rest() {
        // Oracle: `-- a -extra b` ⇒ `["a","b"]` — `-extra` is switch-looking, so it ends
        // the `--` run and then claims everything after itself.
        let (flags, plain) = split_plain(&tokens(&["--", "a", "-extra", "b"]));
        assert!(flags.is_empty());
        assert_eq!(plain, tokens(&["a", "b"]));
    }

    #[test]
    fn a_plus_switch_also_ends_the_end_args_run() {
        // The emulator's own `+`-switches look like switches too (`+A 1` after `--`).
        let (flags, plain) = split_plain(&tokens(&["--", "a", "+A", "1"]));
        assert_eq!(flags, tokens(&["+A", "1"]));
        assert_eq!(plain, tokens(&["a"]));
    }

    #[test]
    fn merge_keeps_the_plain_tails_of_every_entity_in_merge_order() {
        // The documented multi-entity example (`erts/preloaded/src/init.erl:135-139`):
        // `ERL_AFLAGS="-extra a" ERL_ZFLAGS="-extra d" erl -extra b -extra c` ⇒
        // `["a","b","-extra","c","d"]`, with an empty flag stream — each entity splits at
        // its OWN first `-extra`, and the command line's tail carries its later literal
        // `-extra` as data.
        let env = MapEnv(HashMap::from([
            ("ERL_AFLAGS".to_string(), "-extra a".to_string()),
            ("ERL_ZFLAGS".to_string(), "-extra d".to_string()),
        ]));
        let args = tokens(&["-extra", "b", "-extra", "c"]);
        let (flags, plain) = merge_argv_with_plain(&args, &env);
        assert!(flags.is_empty(), "no flag survives: {flags:?}");
        assert_eq!(plain, tokens(&["a", "b", "-extra", "c", "d"]));
        // The flag-stream-only form of the same merge agrees.
        assert_eq!(merge_argv(&args, &env), flags);
    }

    #[test]
    fn an_entity_without_a_marker_is_unaffected_and_stays_in_order() {
        // `ERL_AFLAGS` is prepended and `ERL_ZFLAGS` appended, neither carrying a marker, so
        // the only plain arguments are the command line's own `--` run — and every flag
        // keeps its merge position.
        let env = MapEnv(HashMap::from([
            ("ERL_AFLAGS".to_string(), "-af 1".to_string()),
            ("ERL_FLAGS".to_string(), "-f".to_string()),
            ("ERL_ZFLAGS".to_string(), "-z".to_string()),
        ]));
        let args = tokens(&["-cmd", "--", "a", "b"]);
        let (flags, plain) = merge_argv_with_plain(&args, &env);
        assert_eq!(flags, tokens(&["-af", "1", "-cmd", "-f", "-z"]));
        assert_eq!(plain, tokens(&["a", "b"]));
    }

    #[test]
    fn a_plain_tail_in_two_entities_concatenates_in_merge_order() {
        // A `--` run in `ERL_AFLAGS` and an `-extra` tail on the command line: the two
        // tails concatenate in merge order (AFLAGS first), each entity having split at its
        // own marker.
        let env = MapEnv(HashMap::from([(
            "ERL_AFLAGS".to_string(),
            "-af -- early".to_string(),
        )]));
        let args = tokens(&["--", "mid", "-noshell", "-extra", "late"]);
        let (flags, plain) = merge_argv_with_plain(&args, &env);
        assert_eq!(flags, tokens(&["-af", "-noshell"]));
        assert_eq!(plain, tokens(&["early", "mid", "late"]));
    }
}
