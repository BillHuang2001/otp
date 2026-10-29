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
//! The `-s`/`-run` **Start instruction**'s argument vector — `init`'s
//! `run_args_to_start_instructions/2`.
//!
//! ERTS never applies the collected tokens one by one: `init` folds them into a **single**
//! `{apply, M, F, A}` whose `A` is **one** term — the **list** of the tokens — or is `[]`
//! when no token follows the function (`erts/preloaded/src/init.erl:607-624`):
//!
//! ```text
//! run_args_to_mfa([M]) -> {b2a(M), start, []};
//! run_args_to_mfa([M, F]) -> {b2a(M), b2a(F), []};
//! run_args_to_mfa([M, F | A]) -> {b2a(M), b2a(F), [A]}.
//! run_args_to_start_instructions([], _Converter) -> [];
//! run_args_to_start_instructions(Args, Converter) ->
//!     {M, F, A} = run_args_to_mfa(Args),
//!     [{apply, M, F, map(Converter, A)}].
//! ```
//!
//! `Converter` is `bs2as/1` for `-s` (every token becomes the **atom** it spells) and
//! `bs2ss/1` for `-run` (every token becomes the **string** it spells). So the two flags
//! never differ in *arity*, only in what one token denotes:
//!
//! | Command line | Applied |
//! |---|---|
//! | `-s Mod F` | `Mod:F()` |
//! | `-s Mod F A1 A2` | `Mod:F(['A1','A2'])` |
//! | `-run Mod F` | `Mod:F()` |
//! | `-run Mod F A1 A2` | `Mod:F(["A1","A2"])` |
//!
//! Measured against the oracle (OTP 29.1.2 / erts-17.1.1) on a module whose `f/1` prints
//! its argument: `-s mod f x y` ⇒ `f/1 [x,y]` and `-run mod f x y` ⇒ `f/1 [[120],[121]]`,
//! while `-s mod f` ⇒ `f/0` on both. A numeric token is a **name** too, never a number:
//! `-s mod f 1 2` ⇒ `['1','2']` and `-run mod f 1 2` ⇒ `[[49],[50]]`.
//!
//! One instruction is emitted per flag, in command-line order, so repeated `-s`/`-run`
//! flags are several applies. This CLI keeps a single action slot (the last Start
//! instruction wins, like `-eval`), so [`instruction_args`] describes the argument vector
//! of the one instruction it drives.

use crate::config::ArgTerm;

/// The argument vector ERTS applies for one `-s`/`-run` Start instruction.
///
/// `atoms` selects the flag's converter: `true` for `-s` (tokens become atoms), `false`
/// for `-run` (tokens become strings). Returns **no** argument for an empty token list
/// (the arity-0 form) and otherwise **one** argument, the proper list of the tokens in
/// command-line order.
pub(crate) fn instruction_args(tokens: &[String], atoms: bool) -> Vec<ArgTerm> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let elements = tokens
        .iter()
        .map(|token| {
            if atoms {
                ArgTerm::Atom(token.clone())
            } else {
                ArgTerm::Str(token.clone())
            }
        })
        .collect();
    vec![ArgTerm::List(elements)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn no_token_is_the_arity_zero_form() {
        assert!(instruction_args(&[], true).is_empty());
        assert!(instruction_args(&[], false).is_empty());
    }

    #[test]
    fn the_s_instruction_passes_one_list_of_atoms() {
        assert_eq!(
            instruction_args(&tokens(&["x"]), true),
            vec![ArgTerm::List(vec![ArgTerm::Atom("x".to_string())])]
        );
        assert_eq!(
            instruction_args(&tokens(&["x", "y"]), true),
            vec![ArgTerm::List(vec![
                ArgTerm::Atom("x".to_string()),
                ArgTerm::Atom("y".to_string()),
            ])]
        );
    }

    #[test]
    fn the_run_instruction_passes_one_list_of_strings() {
        assert_eq!(
            instruction_args(&tokens(&["a"]), false),
            vec![ArgTerm::List(vec![ArgTerm::Str("a".to_string())])]
        );
        assert_eq!(
            instruction_args(&tokens(&["a", "b"]), false),
            vec![ArgTerm::List(vec![
                ArgTerm::Str("a".to_string()),
                ArgTerm::Str("b".to_string()),
            ])]
        );
    }

    /// A numeric token is a **name**, never a number: the list element is the atom `'1'`
    /// for `-s` and the string `"1"` (the char list `[49]`) for `-run`.
    #[test]
    fn a_numeric_token_stays_a_name() {
        assert_eq!(
            instruction_args(&tokens(&["1", "2"]), true),
            vec![ArgTerm::List(vec![
                ArgTerm::Atom("1".to_string()),
                ArgTerm::Atom("2".to_string()),
            ])]
        );
        assert_eq!(
            instruction_args(&tokens(&["1", "2"]), false),
            vec![ArgTerm::List(vec![
                ArgTerm::Str("1".to_string()),
                ArgTerm::Str("2".to_string()),
            ])]
        );
    }
}
